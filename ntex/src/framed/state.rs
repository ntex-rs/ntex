//! Framed transport dispatcher
use std::task::{Context, Poll, Waker};
use std::{cell::Cell, cell::RefCell, hash, io, mem, pin::Pin, rc::Rc};

use bytes::BytesMut;
use either::Either;
use futures::{future::poll_fn, ready};

use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed, FramedParts};
use crate::framed::write::flush;
use crate::task::LocalWaker;

const HW: usize = 16 * 1024;
const READ_HW: usize = 8 * 1024;

bitflags::bitflags! {
    pub struct Flags: u16 {
        const DSP_STOP       = 0b0000_0000_0001;
        const DSP_KEEPALIVE  = 0b0000_0000_0010;

        /// io error occured
        const IO_ERR         = 0b0000_0000_0100;
        /// stop io tasks
        const IO_STOP        = 0b0000_0000_1000;
        /// shutdown io tasks
        const IO_SHUTDOWN    = 0b0000_0001_0000;

        /// pause io read
        const RD_PAUSED      = 0b0000_0010_0000;
        /// new data is available
        const RD_READY       = 0b0000_0100_0000;
        /// read buffer is full
        const RD_BUF_FULL    = 0b0000_1000_0000;

        /// write task is ready
        const WR_READY       = 0b0001_0000_0000;
        /// write buffer is full
        const WR_NOT_READY   = 0b0010_0000_0000;

        const ST_DSP_ERR    = 0b0001_0000_0000_0000;
    }
}

pub struct State(Rc<IoStateInner>);

pub(crate) struct IoStateInner {
    flags: Cell<Flags>,
    error: Cell<Option<io::Error>>,
    disconnect_timeout: Cell<u16>,
    read_task: LocalWaker,
    write_task: LocalWaker,
    dispatch_task: LocalWaker,
    read_buf: RefCell<BytesMut>,
    write_buf: RefCell<BytesMut>,
}

impl Clone for State {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Eq for State {}

impl PartialEq for State {
    fn eq(&self, other: &Self) -> bool {
        Rc::as_ptr(&self.0) == Rc::as_ptr(&other.0)
    }
}

impl hash::Hash for State {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        Rc::as_ptr(&self.0).hash(state);
    }
}

impl State {
    #[inline]
    /// Create `State` instance
    pub fn new() -> Self {
        State(Rc::new(IoStateInner {
            flags: Cell::new(Flags::empty()),
            error: Cell::new(None),
            disconnect_timeout: Cell::new(1000),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            read_buf: RefCell::new(BytesMut::new()),
            write_buf: RefCell::new(BytesMut::new()),
        }))
    }

    #[inline]
    /// Create `State` from Framed
    pub fn from_framed<Io, U>(framed: Framed<Io, U>) -> (Io, U, Self) {
        let parts = framed.into_parts();

        let state = State(Rc::new(IoStateInner {
            flags: Cell::new(Flags::empty()),
            error: Cell::new(None),
            disconnect_timeout: Cell::new(1000),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            read_buf: RefCell::new(parts.read_buf),
            write_buf: RefCell::new(parts.write_buf),
        }));
        (parts.io, parts.codec, state)
    }

    #[inline]
    /// Convert state to a Framed instance
    pub fn into_framed<Io, U>(self, io: Io, codec: U) -> Framed<Io, U> {
        let mut parts = FramedParts::new(io, codec);
        parts.read_buf = mem::take(&mut self.0.read_buf.borrow_mut());
        parts.write_buf = mem::take(&mut self.0.write_buf.borrow_mut());
        Framed::from_parts(parts)
    }

    pub(crate) fn keepalive_timeout(&self) {
        let state = self.0.as_ref();
        let mut flags = state.flags.get();
        flags.insert(Flags::DSP_KEEPALIVE);
        state.flags.set(flags);
        state.dispatch_task.wake();
    }

    pub(super) fn disconnect_timeout(&self) -> u16 {
        self.0.disconnect_timeout.get()
    }

    #[inline]
    /// Get current state flags
    pub fn flags(&self) -> Flags {
        self.0.flags.get()
    }

    #[inline]
    /// Set disconnecto timeout
    pub fn set_disconnect_timeout(&self, timeout: u16) {
        self.0.disconnect_timeout.set(timeout)
    }

    #[inline]
    pub fn take_io_error(&self) -> Option<io::Error> {
        self.0.error.take()
    }

    #[inline]
    /// Check if io error occured in read or write task
    pub fn is_io_err(&self) -> bool {
        self.0.flags.get().contains(Flags::IO_ERR)
    }

    #[inline]
    pub fn is_io_shutdown(&self) -> bool {
        self.0
            .flags
            .get()
            .intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN)
    }

    #[inline]
    pub fn is_io_stop(&self) -> bool {
        self.0.flags.get().contains(Flags::IO_STOP)
    }

    #[inline]
    /// Check if write buff is full
    pub fn is_write_buf_full(&self) -> bool {
        self.0.write_buf.borrow().len() >= HW
    }

    #[inline]
    /// Check if read buff is full
    pub fn is_read_buf_full(&self) -> bool {
        self.0.read_buf.borrow().len() >= READ_HW
    }

    #[inline]
    /// Check if read buffer has new data
    pub fn is_read_ready(&self) -> bool {
        self.0.flags.get().contains(Flags::RD_READY)
    }

    /// read task must be paused if service is not ready (RD_PAUSED)
    pub(super) fn is_read_paused(&self) -> bool {
        self.0.flags.get().intersects(Flags::RD_PAUSED)
    }

    #[inline]
    /// Check if write back-pressure is disabled
    pub fn is_write_backpressure_disabled(&self) -> bool {
        let mut flags = self.0.flags.get();
        if flags.contains(Flags::WR_READY) {
            flags.remove(Flags::WR_READY);
            self.0.flags.set(flags);
            true
        } else {
            false
        }
    }

    #[inline]
    /// Check if write back-pressure is enabled
    pub fn is_write_backpressure_enabled(&self) -> bool {
        let mut flags = self.0.flags.get();
        if flags.contains(Flags::WR_READY) {
            flags.remove(Flags::WR_READY);
            self.0.flags.set(flags);
            true
        } else {
            false
        }
    }

    #[inline]
    /// Enable write back-persurre
    pub fn enable_write_backpressure(&self) {
        log::trace!("enable write back-pressure");
        let mut flags = self.0.flags.get();
        flags.insert(Flags::WR_NOT_READY);
        self.0.flags.set(flags);
    }

    #[inline]
    /// Enable read back-persurre
    pub(crate) fn enable_read_backpressure(&self) {
        log::trace!("enable read back-pressure");
        let mut flags = self.0.flags.get();
        flags.insert(Flags::RD_BUF_FULL);
        self.0.flags.set(flags);
    }

    #[inline]
    /// Check if keep-alive timeout occured
    pub fn is_keepalive(&self) -> bool {
        self.0.flags.get().contains(Flags::DSP_KEEPALIVE)
    }

    #[inline]
    /// Reset keep-alive error
    pub fn reset_keepalive(&self) {
        let mut flags = self.0.flags.get();
        flags.remove(Flags::DSP_KEEPALIVE);
        self.0.flags.set(flags);
    }

    #[inline]
    /// Check is dispatcher marked stopped
    pub fn is_dsp_stopped(&self) -> bool {
        self.0.flags.get().contains(Flags::DSP_STOP)
    }

    #[inline]
    pub fn is_open(&self) -> bool {
        !self
            .0
            .flags
            .get()
            .intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN | Flags::DSP_STOP)
    }

    #[inline]
    /// Initiate close connection procedure
    pub fn close(&self) {
        let mut flags = self.0.flags.get();
        flags.insert(Flags::DSP_STOP);
        self.0.flags.set(flags);
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Gracefully shutdown all tasks
    pub fn shutdown(&self) {
        log::trace!("shutdown framed state");
        let mut flags = self.0.flags.get();
        flags.insert(Flags::DSP_STOP | Flags::IO_SHUTDOWN);
        self.0.flags.set(flags);
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Gracefully shutdown read and write io tasks
    pub fn shutdown_io(&self) {
        let mut flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            log::trace!("initiate io shutdown {:?}", flags);
            flags.insert(Flags::IO_SHUTDOWN);
            self.0.flags.set(flags);
            self.0.read_task.wake();
            self.0.write_task.wake();
        }
    }

    pub(crate) fn set_io_error(&self, err: Option<io::Error>) {
        let mut flags = self.0.flags.get();
        self.0.error.set(err);
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
        flags.insert(Flags::IO_ERR | Flags::DSP_STOP);
        self.0.flags.set(flags);
    }

    pub(super) fn set_wr_shutdown_complete(&self) {
        let mut flags = self.0.flags.get();
        flags.insert(Flags::IO_ERR);
        self.0.flags.set(flags);
        self.0.read_task.wake();
    }

    pub(super) fn register_read_task(&self, waker: &Waker) {
        self.0.read_task.register(waker);
    }

    pub(super) fn register_write_task(&self, waker: &Waker) {
        self.0.write_task.register(waker);
    }

    pub(super) fn update_read_task(&self, updated: bool, waker: &Waker) {
        if updated {
            let mut flags = self.0.flags.get();
            flags.insert(Flags::RD_READY);
            self.0.flags.set(flags);
            self.0.dispatch_task.wake();
        }
        self.0.read_task.register(waker);
    }

    pub(super) fn update_write_task(&self) {
        let mut flags = self.0.flags.get();
        if flags.contains(Flags::WR_NOT_READY) {
            flags.remove(Flags::WR_NOT_READY);
            flags.insert(Flags::WR_READY);
            self.0.flags.set(flags);
            self.0.dispatch_task.wake();
        }
    }

    #[inline]
    /// Wake read io task if it is paused
    pub fn dsp_restart_read_task(&self) {
        let mut flags = self.0.flags.get();
        if flags.contains(Flags::RD_PAUSED) {
            flags.remove(Flags::RD_PAUSED);
            self.0.flags.set(flags);
            self.0.read_task.wake();
        }
    }

    #[inline]
    /// Wake write io task
    pub fn dsp_restart_write_task(&self) {
        self.0.write_task.wake();
    }

    #[inline]
    /// Wake read io task if it is not ready
    ///
    /// Only wakes if back-pressure is enabled on read task
    /// otherwise read is already awake.
    pub fn dsp_read_more_data(&self, waker: &Waker) {
        let mut flags = self.0.flags.get();
        flags.remove(Flags::RD_READY);
        self.0.dispatch_task.register(waker);
        if flags.contains(Flags::RD_BUF_FULL) {
            log::trace!("read back-pressure is enabled, wake io task");
            flags.remove(Flags::RD_BUF_FULL);
            self.0.read_task.wake();
        }
        self.0.flags.set(flags);
    }

    #[inline]
    /// Wait until write task flushes data to socket
    ///
    /// Write task must be waken up separately.
    pub fn dsp_enable_write_backpressure(&self, waker: &Waker) {
        let mut flags = self.0.flags.get();
        flags.insert(Flags::WR_NOT_READY);
        self.0.flags.set(flags);
        self.0.dispatch_task.register(waker);
    }

    #[doc(hidden)]
    #[inline]
    /// Mark dispatcher as stopped
    pub fn dsp_mark_stopped(&self) {
        let mut flags = self.0.flags.get();
        flags.insert(Flags::DSP_STOP);
        self.0.flags.set(flags);
    }

    #[inline]
    /// Service is not ready, register dispatch task and
    /// pause read io task
    pub fn dsp_service_not_ready(&self, waker: &Waker) {
        let mut flags = self.0.flags.get();
        flags.insert(Flags::RD_PAUSED);
        self.0.flags.set(flags);
        self.0.dispatch_task.register(waker);
    }

    #[inline]
    /// Stop io tasks
    pub fn dsp_stop_io(&self, waker: &Waker) {
        let mut flags = self.0.flags.get();
        flags.insert(Flags::IO_STOP);
        self.0.flags.set(flags);
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.register(waker);
    }

    #[inline]
    /// Wake dispatcher
    pub fn dsp_wake_task(&self) {
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Register dispatcher task
    pub fn dsp_register_task(&self, waker: &Waker) {
        self.0.dispatch_task.register(waker);
    }

    #[inline]
    /// Reset io stop flags
    pub fn reset_io_stop(&self) {
        let mut flags = self.0.flags.get();
        flags.remove(Flags::IO_STOP);
        self.0.flags.set(flags);
    }

    fn mark_io_error(&self) {
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
        let mut flags = self.0.flags.get();
        flags.insert(Flags::IO_ERR | Flags::DSP_STOP);
        self.0.flags.set(flags);
    }

    #[inline]
    /// Get mut access to read buffer
    pub fn with_read_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        f(&mut self.0.read_buf.borrow_mut())
    }

    #[inline]
    /// Get mut access to write buffer
    pub fn with_write_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        f(&mut self.0.write_buf.borrow_mut())
    }
}

impl State {
    #[inline]
    /// Attempts to decode a frame from the read buffer.
    pub fn decode_item<U>(
        &self,
        codec: &U,
    ) -> Result<Option<<U as Decoder>::Item>, <U as Decoder>::Error>
    where
        U: Decoder,
    {
        codec.decode(&mut self.0.read_buf.borrow_mut())
    }

    #[inline]
    pub async fn next<T, U>(
        &self,
        io: &mut T,
        codec: &U,
    ) -> Result<Option<U::Item>, Either<U::Error, io::Error>>
    where
        T: AsyncRead + AsyncWrite + Unpin,
        U: Decoder,
    {
        loop {
            let item = codec.decode(&mut self.0.read_buf.borrow_mut());
            return match item {
                Ok(Some(el)) => Ok(Some(el)),
                Ok(None) => {
                    let st = self.0.clone();
                    let n = poll_fn(|cx| {
                        Pin::new(&mut *io)
                            .poll_read_buf(cx, &mut *st.read_buf.borrow_mut())
                    })
                    .await
                    .map_err(Either::Right)?;
                    if n == 0 {
                        Ok(None)
                    } else {
                        continue;
                    }
                }
                Err(err) => {
                    self.mark_io_error();
                    Err(Either::Left(err))
                }
            };
        }
    }

    #[inline]
    pub fn poll_next<T, U>(
        &self,
        io: &mut T,
        codec: &U,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<U::Item>, Either<U::Error, io::Error>>>
    where
        T: AsyncRead + AsyncWrite + Unpin,
        U: Decoder,
    {
        let mut buf = self.0.read_buf.borrow_mut();

        loop {
            return match codec.decode(&mut buf) {
                Ok(Some(el)) => Poll::Ready(Ok(Some(el))),
                Ok(None) => {
                    let n = ready!(Pin::new(&mut *io).poll_read_buf(cx, &mut *buf))
                        .map_err(Either::Right)?;
                    if n == 0 {
                        Poll::Ready(Ok(None))
                    } else {
                        continue;
                    }
                }
                Err(err) => {
                    self.mark_io_error();
                    Poll::Ready(Err(Either::Left(err)))
                }
            };
        }
    }

    #[inline]
    /// Encode item, send to a peer and flush
    pub async fn send<T, U>(
        &self,
        io: &mut T,
        codec: &U,
        item: U::Item,
    ) -> Result<(), Either<U::Error, io::Error>>
    where
        T: AsyncRead + AsyncWrite + Unpin,
        U: Encoder,
    {
        codec
            .encode(item, &mut self.0.write_buf.borrow_mut())
            .map_err(Either::Left)?;

        let st = self.0.clone();
        poll_fn(|cx| flush(io, &mut st.write_buf.borrow_mut(), cx))
            .await
            .map_err(|e| {
                self.mark_io_error();
                Either::Right(e)
            })
    }

    #[inline]
    /// Write item to a buf and wake up io task
    ///
    /// Returns state of write buffer state, false is returned if write buffer if full.
    pub fn write_item<U>(
        &self,
        item: U::Item,
        codec: &U,
    ) -> Result<bool, <U as Encoder>::Error>
    where
        U: Encoder,
    {
        let flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            let mut write_buf = self.0.write_buf.borrow_mut();
            let is_write_sleep = write_buf.is_empty();

            // encode item and wake write task
            codec.encode(item, &mut *write_buf).map(|_| {
                if is_write_sleep {
                    self.0.write_task.wake();
                }
                write_buf.len() < HW
            })
        } else {
            Ok(true)
        }
    }

    #[inline]
    /// Write item to a buf and wake up io task
    pub fn write_result<U, E>(
        &self,
        item: Result<Option<U::Item>, E>,
        codec: &U,
    ) -> Result<bool, Either<E, U::Error>>
    where
        U: Encoder,
    {
        let mut flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::ST_DSP_ERR) {
            match item {
                Ok(Some(item)) => {
                    let mut write_buf = self.0.write_buf.borrow_mut();
                    let is_write_sleep = write_buf.is_empty();

                    // encode item
                    if let Err(err) = codec.encode(item, &mut write_buf) {
                        log::trace!("Codec encoder error: {:?}", err);
                        flags.insert(Flags::DSP_STOP | Flags::ST_DSP_ERR);
                        self.0.flags.set(flags);
                        self.0.dispatch_task.wake();
                        return Err(Either::Right(err));
                    } else if is_write_sleep {
                        self.0.write_task.wake();
                    }
                    Ok(write_buf.len() < HW)
                }
                Err(err) => {
                    flags.insert(Flags::DSP_STOP | Flags::ST_DSP_ERR);
                    self.0.flags.set(flags);
                    self.0.dispatch_task.wake();
                    Err(Either::Left(err))
                }
                _ => Ok(true),
            }
        } else {
            Ok(true)
        }
    }
}
