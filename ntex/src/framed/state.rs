//! Framed transport dispatcher
use std::task::{Context, Poll, Waker};
use std::{cell::Cell, cell::RefCell, future::Future, hash, io, mem, pin::Pin, rc::Rc};

use slab::Slab;

use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed, FramedParts};
use crate::task::LocalWaker;
use crate::util::{poll_fn, Buf, BytesMut, Either};

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

        /// write buffer is full
        const WR_BACKPRESSURE = 0b0000_0001_0000_0000;

        const ST_DSP_ERR      = 0b0001_0000_0000_0000;
    }
}

pub struct State(Rc<IoStateInner>);

pub(crate) struct IoStateInner {
    flags: Cell<Flags>,
    lw: Cell<u16>,
    read_hw: Cell<u16>,
    write_hw: Cell<u16>,
    disconnect_timeout: Cell<u16>,
    error: Cell<Option<io::Error>>,
    read_task: LocalWaker,
    write_task: LocalWaker,
    dispatch_task: LocalWaker,
    read_buf: RefCell<BytesMut>,
    write_buf: RefCell<BytesMut>,
    on_disconnect: RefCell<Slab<Option<LocalWaker>>>,
}

impl IoStateInner {
    fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    fn remove_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.remove(f);
        self.flags.set(flags);
    }
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
            lw: Cell::new(1024),
            read_hw: Cell::new(8 * 1024),
            write_hw: Cell::new(8 * 1024),
            disconnect_timeout: Cell::new(1),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            read_buf: RefCell::new(BytesMut::new()),
            write_buf: RefCell::new(BytesMut::new()),
            on_disconnect: RefCell::new(Slab::new()),
        }))
    }

    #[inline]
    /// Create `State` from Framed
    pub fn from_framed<Io, U>(framed: Framed<Io, U>) -> (Io, U, Self) {
        let parts = framed.into_parts();

        let state = State(Rc::new(IoStateInner {
            flags: Cell::new(Flags::empty()),
            error: Cell::new(None),
            lw: Cell::new(1024),
            read_hw: Cell::new(8 * 1024),
            write_hw: Cell::new(8 * 1024),
            disconnect_timeout: Cell::new(1),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            read_buf: RefCell::new(parts.read_buf),
            write_buf: RefCell::new(parts.write_buf),
            on_disconnect: RefCell::new(Slab::new()),
        }));
        (parts.io, parts.codec, state)
    }

    #[inline]
    /// Create `State` instance with custom params
    pub fn with_params(
        max_read_buf_size: u16,
        max_write_buf_size: u16,
        min_buf_size: u16,
        disconnect_timeout: u16,
    ) -> Self {
        State(Rc::new(IoStateInner {
            flags: Cell::new(Flags::empty()),
            error: Cell::new(None),
            lw: Cell::new(min_buf_size),
            read_hw: Cell::new(max_read_buf_size),
            write_hw: Cell::new(max_write_buf_size),
            disconnect_timeout: Cell::new(disconnect_timeout),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            read_buf: RefCell::new(BytesMut::with_capacity(min_buf_size as usize)),
            write_buf: RefCell::new(BytesMut::with_capacity(min_buf_size as usize)),
            on_disconnect: RefCell::new(Slab::new()),
        }))
    }

    #[inline]
    /// Convert State to a Framed instance
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

    pub(super) fn get_disconnect_timeout(&self) -> u16 {
        self.0.disconnect_timeout.get()
    }

    fn insert_flags(&self, f: Flags) {
        let mut flags = self.0.flags.get();
        flags.insert(f);
        self.0.flags.set(flags);
    }

    fn remove_flags(&self, f: Flags) {
        let mut flags = self.0.flags.get();
        flags.remove(f);
        self.0.flags.set(flags);
    }

    #[inline]
    #[doc(hidden)]
    /// Get current state flags
    pub fn flags(&self) -> Flags {
        self.0.flags.get()
    }

    #[inline]
    /// Set read/write buffer sizes
    ///
    /// By default read max buf size is 8kb, write max buf size is 8kb
    pub fn set_buffer_params(
        &self,
        max_read_buf_size: u16,
        max_write_buf_size: u16,
        min_buf_size: u16,
    ) {
        self.0.read_hw.set(max_read_buf_size);
        self.0.write_hw.set(max_write_buf_size);
        self.0.lw.set(min_buf_size);
    }

    #[inline]
    /// Set io disconnect timeout in secs
    pub fn set_disconnect_timeout(&self, timeout: u16) {
        self.0.disconnect_timeout.set(timeout)
    }

    #[inline]
    /// Notify when socket get disconnected
    pub fn on_disconnect(&self) -> OnDisconnect {
        OnDisconnect::new(self.0.clone(), self.0.flags.get().contains(Flags::IO_ERR))
    }

    fn notify_disconnect(&self) {
        let mut slab = self.0.on_disconnect.borrow_mut();
        for item in slab.iter_mut() {
            if let Some(waker) = item.1 {
                waker.wake();
            } else {
                *item.1 = Some(LocalWaker::default())
            }
        }
    }

    #[inline]
    /// Check if io error occured in read or write task
    pub fn is_io_err(&self) -> bool {
        self.0.flags.get().contains(Flags::IO_ERR)
    }

    pub(super) fn is_io_shutdown(&self) -> bool {
        self.0
            .flags
            .get()
            .intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN)
    }

    pub(super) fn is_io_stop(&self) -> bool {
        self.0.flags.get().contains(Flags::IO_STOP)
    }

    pub(super) fn is_read_paused(&self) -> bool {
        self.0.flags.get().contains(Flags::RD_PAUSED)
    }

    #[inline]
    /// Check if keep-alive timeout occured
    pub fn is_keepalive(&self) -> bool {
        self.0.flags.get().contains(Flags::DSP_KEEPALIVE)
    }

    #[inline]
    /// Check is dispatcher marked stopped
    pub fn is_dispatcher_stopped(&self) -> bool {
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

    pub(crate) fn set_io_error(&self, err: Option<io::Error>) {
        self.0.error.set(err);
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
        self.insert_flags(Flags::IO_ERR | Flags::DSP_STOP);
        self.notify_disconnect();
    }

    pub(super) fn set_wr_shutdown_complete(&self) {
        if !self.0.flags.get().contains(Flags::IO_ERR) {
            self.notify_disconnect();
            self.insert_flags(Flags::IO_ERR);
            self.0.read_task.wake();
        }
    }

    pub(super) fn register_read_task(&self, waker: &Waker) {
        self.0.read_task.register(waker);
    }

    #[inline]
    /// Stop io tasks
    ///
    /// Wake dispatcher when read or write task is stopped.
    pub fn stop_io(&self, waker: &Waker) {
        self.insert_flags(Flags::IO_STOP);
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.register(waker);
    }

    #[inline]
    /// Gracefully shutdown read and write io tasks
    pub fn shutdown_io(&self) {
        let flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            log::trace!("initiate io shutdown {:?}", flags);
            self.insert_flags(Flags::IO_SHUTDOWN);
            self.0.read_task.wake();
            self.0.write_task.wake();
        }
    }

    #[inline]
    /// Take io error if any occured
    pub fn take_io_error(&self) -> Option<io::Error> {
        self.0.error.take()
    }

    #[inline]
    /// Reset io stop flags
    pub fn reset_io_stop(&self) {
        self.remove_flags(Flags::IO_STOP);
    }

    #[inline]
    /// Reset keep-alive error
    pub fn reset_keepalive(&self) {
        self.remove_flags(Flags::DSP_KEEPALIVE)
    }

    #[inline]
    /// Wake dispatcher task
    pub fn wake_dispatcher(&self) {
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Register dispatcher task
    pub fn register_dispatcher(&self, waker: &Waker) {
        self.0.dispatch_task.register(waker);
    }

    #[inline]
    /// Mark dispatcher as stopped
    pub fn dispatcher_stopped(&self) {
        self.insert_flags(Flags::DSP_STOP);
    }

    #[inline]
    /// Get api for read task
    pub fn read(&'_ self) -> Read<'_> {
        Read(self.0.as_ref())
    }

    #[inline]
    /// Get api for write task
    pub fn write(&'_ self) -> Write<'_> {
        Write(self.0.as_ref())
    }

    #[inline]
    /// Gracefully close connection
    ///
    /// First stop dispatcher, then dispatcher stops io tasks
    pub fn close(&self) {
        self.insert_flags(Flags::DSP_STOP);
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Force close connection
    ///
    /// Dispatcher does not wait for uncompleted responses, but flushes io buffers.
    pub fn force_close(&self) {
        log::trace!("force close framed object");
        self.insert_flags(Flags::DSP_STOP | Flags::IO_SHUTDOWN);
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
    }
}

impl State {
    #[inline]
    /// Read incoming io stream and decode codec item.
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
                        crate::codec::poll_read_buf(
                            Pin::new(&mut *io),
                            cx,
                            &mut *st.read_buf.borrow_mut(),
                        )
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
                    self.set_io_error(None);
                    Err(Either::Left(err))
                }
            };
        }
    }

    #[inline]
    /// Encode item, send to a peer and then flush
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

        if !poll_fn(|cx| self.flush_io(io, cx)).await {
            let err = self.0.error.take().unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "Internal error")
            });
            Err(Either::Right(err))
        } else {
            Ok(())
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
                    match crate::codec::poll_read_buf(Pin::new(&mut *io), cx, &mut *buf)
                    {
                        Poll::Pending => Poll::Pending,
                        Poll::Ready(Err(err)) => Poll::Ready(Err(Either::Right(err))),
                        Poll::Ready(Ok(n)) => {
                            if n == 0 {
                                Poll::Ready(Ok(None))
                            } else {
                                continue;
                            }
                        }
                    }
                }
                Err(err) => {
                    self.set_io_error(None);
                    Poll::Ready(Err(Either::Left(err)))
                }
            };
        }
    }

    /// read data from io steram and update internal state
    pub(super) fn read_io<T>(&self, io: &mut T, cx: &mut Context<'_>) -> bool
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        let inner = self.0.as_ref();
        let lw = inner.lw.get() as usize;
        let buf = &mut inner.read_buf.borrow_mut();

        // read data from socket
        let mut updated = false;
        loop {
            // make sure we've got room
            let remaining = buf.capacity() - buf.len();
            if remaining < lw {
                buf.reserve((inner.read_hw.get() as usize) - remaining);
            }

            match crate::codec::poll_read_buf(Pin::new(&mut *io), cx, buf) {
                Poll::Pending => break,
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        log::trace!("io stream is disconnected");
                        self.set_io_error(None);
                        return false;
                    } else {
                        if buf.len() > inner.read_hw.get() as usize {
                            log::trace!(
                                "buffer is too large {}, enable read back-pressure",
                                buf.len()
                            );
                            self.insert_flags(Flags::RD_READY | Flags::RD_BUF_FULL);
                            self.0.dispatch_task.wake();
                            self.0.read_task.register(cx.waker());
                            return true;
                        }

                        updated = true;
                    }
                }
                Poll::Ready(Err(err)) => {
                    log::trace!("read task failed on io {:?}", err);
                    self.set_io_error(Some(err));
                    return false;
                }
            }
        }

        if updated {
            self.insert_flags(Flags::RD_READY);
            self.0.dispatch_task.wake();
        }
        self.0.read_task.register(cx.waker());
        true
    }

    /// Flush write buffer to underlying I/O stream.
    pub(super) fn flush_io<T>(&self, io: &mut T, cx: &mut Context<'_>) -> Poll<bool>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        let inner = self.0.as_ref();
        let buf = &mut inner.write_buf.borrow_mut();
        let len = buf.len();

        if len != 0 {
            // log::trace!("flushing framed transport: {}", len);

            let mut written = 0;
            while written < len {
                match Pin::new(&mut *io).poll_write(cx, &buf[written..]) {
                    Poll::Pending => break,
                    Poll::Ready(Ok(n)) => {
                        if n == 0 {
                            log::trace!(
                                "Disconnected during flush, written {}",
                                written
                            );
                            self.set_io_error(Some(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "failed to write frame to transport",
                            )));
                            return Poll::Ready(false);
                        } else {
                            written += n
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        log::trace!("Error during flush: {}", e);
                        self.set_io_error(Some(e));
                        return Poll::Ready(false);
                    }
                }
            }
            // log::trace!("flushed {} bytes", written);

            // remove written data
            if written == len {
                buf.clear()
            } else {
                buf.advance(written);
            }
        }

        // if write buffer is smaller than high watermark value, turn off back-pressure
        if buf.len() < self.0.write_hw.get() as usize {
            let mut flags = self.0.flags.get();
            if flags.contains(Flags::WR_BACKPRESSURE) {
                flags.remove(Flags::WR_BACKPRESSURE);
                self.0.flags.set(flags);
                self.0.dispatch_task.wake();
            }
        } else {
            self.insert_flags(Flags::WR_BACKPRESSURE);
        }
        self.0.write_task.register(cx.waker());

        // flush
        match Pin::new(&mut *io).poll_flush(cx) {
            Poll::Ready(Ok(_)) => {
                if buf.is_empty() {
                    Poll::Ready(true)
                } else {
                    Poll::Pending
                }
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => {
                log::trace!("Error during flush: {}", err);
                self.set_io_error(Some(err));
                Poll::Ready(false)
            }
        }
    }
}

#[derive(Copy, Clone)]
pub struct Write<'a>(&'a IoStateInner);

impl<'a> Write<'a> {
    #[inline]
    /// Check if write task is ready
    pub fn is_ready(&self) -> bool {
        !self.0.flags.get().contains(Flags::WR_BACKPRESSURE)
    }

    #[inline]
    /// Check if write buffer is full
    pub fn is_full(&self) -> bool {
        self.0.write_buf.borrow().len() >= self.0.write_hw.get() as usize
    }

    #[inline]
    /// Wait until write task flushes data to io stream
    ///
    /// Write task must be waken up separately.
    pub fn enable_backpressure(&self, waker: Option<&Waker>) {
        log::trace!("enable write back-pressure");
        self.0.insert_flags(Flags::WR_BACKPRESSURE);
        if let Some(waker) = waker {
            self.0.dispatch_task.register(waker);
        }
    }

    #[inline]
    /// Wake dispatcher task
    pub fn wake_dispatcher(&self) {
        self.0.dispatch_task.wake();
    }

    /// Get mut access to write buffer
    pub fn with_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let mut write_buf = self.0.write_buf.borrow_mut();
        if write_buf.is_empty() {
            self.0.write_task.wake();
        }

        f(&mut write_buf)
    }

    #[inline]
    /// Write item to a buffer and wake up write task
    ///
    /// Returns write buffer state, false is returned if write buffer if full.
    pub fn encode<U>(
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

            // make sure we've got room
            let remaining = write_buf.capacity() - write_buf.len();
            if remaining < self.0.lw.get() as usize {
                write_buf.reserve((self.0.write_hw.get() as usize) - remaining);
            }

            // encode item and wake write task
            codec.encode(item, &mut *write_buf).map(|_| {
                if is_write_sleep {
                    self.0.write_task.wake();
                }
                write_buf.len() < self.0.write_hw.get() as usize
            })
        } else {
            Ok(true)
        }
    }

    #[inline]
    /// Write item to a buf and wake up io task
    pub fn encode_result<U, E>(
        &self,
        item: Result<Option<U::Item>, E>,
        codec: &U,
    ) -> Result<bool, Either<E, U::Error>>
    where
        U: Encoder,
    {
        let flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::ST_DSP_ERR) {
            match item {
                Ok(Some(item)) => {
                    let mut write_buf = self.0.write_buf.borrow_mut();
                    let is_write_sleep = write_buf.is_empty();

                    // make sure we've got room
                    let remaining = write_buf.capacity() - write_buf.len();
                    if remaining < self.0.lw.get() as usize {
                        write_buf.reserve((self.0.write_hw.get() as usize) - remaining);
                    }

                    // encode item
                    if let Err(err) = codec.encode(item, &mut write_buf) {
                        log::trace!("Encoder error: {:?}", err);
                        self.0.insert_flags(Flags::DSP_STOP | Flags::ST_DSP_ERR);
                        self.0.dispatch_task.wake();
                        return Err(Either::Right(err));
                    } else if is_write_sleep {
                        self.0.write_task.wake();
                    }
                    Ok(write_buf.len() < self.0.write_hw.get() as usize)
                }
                Err(err) => {
                    self.0.insert_flags(Flags::DSP_STOP | Flags::ST_DSP_ERR);
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

#[derive(Copy, Clone)]
pub struct Read<'a>(&'a IoStateInner);

impl<'a> Read<'a> {
    #[inline]
    /// Check if read buffer has new data
    pub fn is_ready(&self) -> bool {
        self.0.flags.get().contains(Flags::RD_READY)
    }

    #[inline]
    /// Check if read buffer is full
    pub fn is_full(&self) -> bool {
        self.0.read_buf.borrow().len() >= self.0.read_hw.get() as usize
    }

    #[inline]
    /// Pause read task
    ///
    /// Also register dispatch task
    pub fn pause(&self, waker: &Waker) {
        self.0.insert_flags(Flags::RD_PAUSED);
        self.0.dispatch_task.register(waker);
    }

    #[inline]
    /// Wake read io task if it is paused
    pub fn resume(&self) {
        let flags = self.0.flags.get();
        if flags.contains(Flags::RD_PAUSED) {
            self.0.remove_flags(Flags::RD_PAUSED);
            self.0.read_task.wake();
        }
    }

    #[inline]
    /// Wake read task and instruct to read more data
    ///
    /// Only wakes if back-pressure is enabled on read task
    /// otherwise read is already awake.
    pub fn wake(&self, waker: &Waker) {
        let mut flags = self.0.flags.get();
        flags.remove(Flags::RD_READY);
        if flags.contains(Flags::RD_BUF_FULL) {
            log::trace!("read back-pressure is enabled, wake io task");
            flags.remove(Flags::RD_BUF_FULL);
            self.0.read_task.wake();
        }
        self.0.flags.set(flags);
        self.0.dispatch_task.register(waker);
    }

    #[inline]
    /// Attempts to decode a frame from the read buffer.
    pub fn decode<U>(
        &self,
        codec: &U,
    ) -> Result<Option<<U as Decoder>::Item>, <U as Decoder>::Error>
    where
        U: Decoder,
    {
        codec.decode(&mut self.0.read_buf.borrow_mut())
    }

    /// Get mut access to read buffer
    pub fn with_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        f(&mut self.0.read_buf.borrow_mut())
    }
}

/// OnDisconnect future resolves when socket get disconnected
#[must_use = "OnDisconnect do nothing unless polled"]
pub struct OnDisconnect {
    token: usize,
    inner: Rc<IoStateInner>,
}

impl OnDisconnect {
    fn new(inner: Rc<IoStateInner>, disconnected: bool) -> Self {
        let token = inner.on_disconnect.borrow_mut().insert(if disconnected {
            Some(LocalWaker::default())
        } else {
            None
        });
        Self { token, inner }
    }

    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut on_disconnect = self.inner.on_disconnect.borrow_mut();

        let inner = unsafe { on_disconnect.get_unchecked_mut(self.token) };
        if inner.is_none() {
            let waker = LocalWaker::default();
            waker.register(cx.waker());
            *inner = Some(waker);
        } else if !inner.as_mut().unwrap().register(cx.waker()) {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Clone for OnDisconnect {
    fn clone(&self) -> Self {
        let token = self.inner.on_disconnect.borrow_mut().insert(None);
        OnDisconnect {
            token,
            inner: self.inner.clone(),
        }
    }
}

impl Future for OnDisconnect {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_ready(cx)
    }
}

impl Drop for OnDisconnect {
    fn drop(&mut self) {
        self.inner.on_disconnect.borrow_mut().remove(self.token);
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::{codec::BytesCodec, testing::Io, util::lazy};

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[crate::rt_test]
    async fn test_utils() {
        let (client, mut server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write(TEXT);

        let state = State::new();
        assert!(!state.read().is_full());
        assert!(!state.write().is_full());

        let msg = state.next(&mut server, &BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        let res =
            poll_fn(|cx| Poll::Ready(state.poll_next(&mut server, &BytesCodec, cx)))
                .await;
        assert!(res.is_pending());
        client.write(TEXT);
        let res =
            poll_fn(|cx| Poll::Ready(state.poll_next(&mut server, &BytesCodec, cx)))
                .await;
        if let Poll::Ready(msg) = res {
            assert_eq!(msg.unwrap().unwrap(), Bytes::from_static(BIN));
        }

        client.read_error(io::Error::new(io::ErrorKind::Other, "err"));
        let msg = state.next(&mut server, &BytesCodec).await;
        assert!(msg.is_err());
        state.flags().contains(Flags::IO_ERR);
        state.flags().contains(Flags::DSP_STOP);
        state.remove_flags(Flags::IO_ERR | Flags::DSP_STOP);

        client.read_error(io::Error::new(io::ErrorKind::Other, "err"));
        let res =
            poll_fn(|cx| Poll::Ready(state.poll_next(&mut server, &BytesCodec, cx)))
                .await;
        if let Poll::Ready(msg) = res {
            assert!(msg.is_err());
            state.flags().contains(Flags::IO_ERR);
            state.flags().contains(Flags::DSP_STOP);
            state.remove_flags(Flags::IO_ERR | Flags::DSP_STOP);
        }

        state
            .send(&mut server, &BytesCodec, Bytes::from_static(b"test"))
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write_error(io::Error::new(io::ErrorKind::Other, "err"));
        let res = state
            .send(&mut server, &BytesCodec, Bytes::from_static(b"test"))
            .await;
        assert!(res.is_err());
        state.flags().contains(Flags::IO_ERR);
        state.flags().contains(Flags::DSP_STOP);
        state.remove_flags(Flags::IO_ERR | Flags::DSP_STOP);

        state.remove_flags(Flags::IO_ERR | Flags::DSP_STOP);
        state.force_close();
        state.flags().contains(Flags::DSP_STOP);
        state.flags().contains(Flags::IO_SHUTDOWN);
    }

    #[crate::rt_test]
    async fn test_on_disconnect() {
        let state = State::new();
        let mut waiter = state.on_disconnect();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Pending
        );
        let mut waiter2 = waiter.clone();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter2).poll(cx)).await,
            Poll::Pending
        );
        state.set_wr_shutdown_complete();
        assert_eq!(waiter.await, ());
        assert_eq!(waiter2.await, ());

        let mut waiter = state.on_disconnect();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Ready(())
        );

        let state = State::new();
        let mut waiter = state.on_disconnect();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Pending
        );
        state.set_io_error(None);
        assert_eq!(waiter.await, ());
    }
}
