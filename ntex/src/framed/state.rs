//! Framed transport dispatcher
use std::task::{Context, Poll, Waker};
use std::{cell::Cell, cell::RefCell, fmt, hash, io, mem, pin::Pin, rc::Rc};

use bytes::BytesMut;
use either::Either;
use futures::{future::poll_fn, ready};

use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder};
use crate::framed::write::flush;
use crate::task::LocalWaker;

type Request<U> = <U as Decoder>::Item;
type Response<U> = <U as Encoder>::Item;

const LW: usize = 1024;
const HW: usize = 8 * 1024;

bitflags::bitflags! {
    pub(crate) struct Flags: u8 {
        const DSP_STOP       = 0b0000_0001;
        const DSP_KEEPALIVE  = 0b0000_0100;

        const IO_ERR         = 0b0000_1000;
        const IO_SHUTDOWN    = 0b0001_0000;

        /// pause io read
        const RD_PAUSED      = 0b0010_0000;
        /// new data is available
        const RD_READY       = 0b0100_0000;

        const ST_DSP_ERR     = 0b1000_0000;
    }
}

/// Framed transport item
pub enum DispatcherItem<U: Encoder + Decoder> {
    Item(Request<U>),
    /// Keep alive timeout
    KeepAliveTimeout,
    /// Decoder parse error
    DecoderError(<U as Decoder>::Error),
    /// Encoder parse error
    EncoderError(<U as Encoder>::Error),
    /// Unexpected io error
    IoError(io::Error),
}

impl<U> fmt::Debug for DispatcherItem<U>
where
    U: Encoder + Decoder,
    <U as Decoder>::Item: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DispatcherItem::Item(ref item) => {
                write!(fmt, "DispatcherItem::Item({:?})", item)
            }
            DispatcherItem::KeepAliveTimeout => {
                write!(fmt, "DispatcherItem::KeepAliveTimeout")
            }
            DispatcherItem::EncoderError(ref e) => {
                write!(fmt, "DispatcherItem::EncoderError({:?})", e)
            }
            DispatcherItem::DecoderError(ref e) => {
                write!(fmt, "DispatcherItem::DecoderError({:?})", e)
            }
            DispatcherItem::IoError(ref e) => {
                write!(fmt, "DispatcherItem::IoError({:?})", e)
            }
        }
    }
}

pub struct State<U>(Rc<IoStateInner<U>>);

pub(crate) struct IoStateInner<U> {
    codec: RefCell<U>,
    flags: Cell<Flags>,
    error: Cell<Option<io::Error>>,
    disconnect_timeout: Cell<u16>,
    read_task: LocalWaker,
    write_task: LocalWaker,
    dispatch_task: LocalWaker,
    read_buf: RefCell<BytesMut>,
    write_buf: RefCell<BytesMut>,
}

impl<U> State<U> {
    pub(crate) fn keepalive_timeout(&self) {
        let state = self.0.as_ref();
        let mut flags = state.flags.get();
        flags.insert(Flags::DSP_STOP | Flags::DSP_KEEPALIVE);
        state.flags.set(flags);
        state.dispatch_task.wake();
    }
}

impl<U> Clone for State<U> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<U> Eq for State<U> {}

impl<U> PartialEq for State<U> {
    fn eq(&self, other: &Self) -> bool {
        Rc::as_ptr(&self.0) == Rc::as_ptr(&other.0)
    }
}

impl<U> hash::Hash for State<U> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        Rc::as_ptr(&self.0).hash(state);
    }
}

impl<U> State<U> {
    /// Create `State` instance
    pub fn new(codec: U) -> Self {
        State(Rc::new(IoStateInner {
            flags: Cell::new(Flags::empty()),
            error: Cell::new(None),
            disconnect_timeout: Cell::new(1000),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            codec: RefCell::new(codec),
            read_buf: RefCell::new(BytesMut::new()),
            write_buf: RefCell::new(BytesMut::new()),
        }))
    }

    pub(super) fn disconnect_timeout(&self) -> u16 {
        self.0.disconnect_timeout.get()
    }

    #[inline]
    pub fn set_disconnect_timeout(&self, timeout: u16) {
        self.0.disconnect_timeout.set(timeout)
    }

    #[inline]
    pub fn take_io_error(&self) -> Option<io::Error> {
        self.0.error.take()
    }

    #[inline]
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
    pub fn is_read_ready(&self) -> bool {
        self.0.flags.get().contains(Flags::RD_READY)
    }

    pub(super) fn is_read_paused(&self) -> bool {
        self.0.flags.get().contains(Flags::RD_PAUSED)
    }

    #[inline]
    pub fn is_keepalive_err(&self) -> bool {
        self.0.flags.get().contains(Flags::DSP_KEEPALIVE)
    }

    #[inline]
    pub fn is_dsp_stopped(&self) -> bool {
        self.0.flags.get().contains(Flags::DSP_STOP)
    }

    #[inline]
    pub fn is_opened(&self) -> bool {
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
    /// Shutdown all tasks
    pub fn shutdown(&self) {
        let mut flags = self.0.flags.get();
        flags.insert(Flags::DSP_STOP | Flags::IO_SHUTDOWN);
        self.0.flags.set(flags);
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Shutdown read and write io tasks
    pub fn shutdown_io(&self) {
        log::trace!("initiate io shutdown {:?}", self.0.flags.get());
        let mut flags = self.0.flags.get();
        flags.insert(Flags::IO_SHUTDOWN);
        self.0.flags.set(flags);
        self.0.read_task.wake();
        self.0.write_task.wake();
    }

    pub(crate) fn set_io_error(&self, err: Option<io::Error>) {
        self.0.error.set(err);
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
        let mut flags = self.0.flags.get();
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
    /// Wake read io task if it is not ready
    pub fn dsp_read_more_data(&self, waker: &Waker) {
        let mut flags = self.0.flags.get();
        flags.remove(Flags::RD_READY);
        self.0.flags.set(flags);
        self.0.read_task.wake();
        self.0.dispatch_task.register(waker);
    }

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
    /// Wake dispatcher
    pub fn dsp_wake_task(&self) {
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Register dispatcher task
    pub fn dsp_register_task(&self, waker: &Waker) {
        self.0.dispatch_task.register(waker);
    }

    pub(crate) fn with_read_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        f(&mut self.0.read_buf.borrow_mut())
    }

    pub(crate) fn with_write_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        f(&mut self.0.write_buf.borrow_mut())
    }
}

impl<U> State<U>
where
    U: Encoder + Decoder,
{
    #[inline]
    /// Consume the `IoState`, returning `IoState` with different codec.
    pub fn map_codec<F, U2>(self, f: F) -> State<U2>
    where
        F: Fn(&U) -> U2,
        U2: Encoder + Decoder,
    {
        let st = self.0.as_ref();
        let codec = f(&st.codec.borrow());

        State(Rc::new(IoStateInner {
            codec: RefCell::new(codec),
            flags: Cell::new(st.flags.get()),
            error: Cell::new(st.error.take()),
            disconnect_timeout: Cell::new(st.disconnect_timeout.get()),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            read_buf: RefCell::new(mem::take(&mut st.read_buf.borrow_mut())),
            write_buf: RefCell::new(mem::take(&mut st.write_buf.borrow_mut())),
        }))
    }

    #[inline]
    pub fn with_codec<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut U) -> R,
    {
        f(&mut *self.0.codec.borrow_mut())
    }

    #[inline]
    pub async fn next<T>(
        &self,
        io: &mut T,
    ) -> Result<Option<<U as Decoder>::Item>, Either<<U as Decoder>::Error, io::Error>>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            let item = {
                self.0
                    .codec
                    .borrow_mut()
                    .decode(&mut self.0.read_buf.borrow_mut())
            };
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
                Err(err) => Err(Either::Left(err)),
            };
        }
    }

    #[inline]
    pub fn poll_next<T>(
        &self,
        io: &mut T,
        cx: &mut Context<'_>,
    ) -> Poll<
        Result<Option<<U as Decoder>::Item>, Either<<U as Decoder>::Error, io::Error>>,
    >
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        let mut buf = self.0.read_buf.borrow_mut();
        let mut codec = self.0.codec.borrow_mut();

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
                Err(err) => Poll::Ready(Err(Either::Left(err))),
            };
        }
    }

    #[inline]
    pub async fn send<T>(
        &self,
        io: &mut T,
        item: <U as Encoder>::Item,
    ) -> Result<(), Either<<U as Encoder>::Error, io::Error>>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        self.0
            .codec
            .borrow_mut()
            .encode(item, &mut self.0.write_buf.borrow_mut())
            .map_err(Either::Left)?;

        let st = self.0.clone();
        poll_fn(|cx| flush(io, &mut st.write_buf.borrow_mut(), cx))
            .await
            .map_err(Either::Right)
    }

    #[inline]
    /// Attempts to decode a frame from the read buffer.
    pub fn decode_item(
        &self,
    ) -> Result<Option<<U as Decoder>::Item>, <U as Decoder>::Error> {
        self.0
            .codec
            .borrow_mut()
            .decode(&mut self.0.read_buf.borrow_mut())
    }

    #[inline]
    /// Write item to a buf and wake up io task
    pub fn write_item(
        &self,
        item: <U as Encoder>::Item,
    ) -> Result<(), <U as Encoder>::Error> {
        let flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            let mut write_buf = self.0.write_buf.borrow_mut();
            let is_write_sleep = write_buf.is_empty();

            // encode item and wake write task
            let res = self.0.codec.borrow_mut().encode(item, &mut *write_buf);
            if res.is_ok() && is_write_sleep {
                self.0.write_task.wake();
            }
            res
        } else {
            Ok(())
        }
    }

    #[inline]
    /// Write item to a buf and wake up io task
    pub fn write_result<E>(
        &self,
        item: Result<Option<Response<U>>, E>,
    ) -> Result<(), Either<E, <U as Encoder>::Error>> {
        let mut flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::ST_DSP_ERR) {
            match item {
                Ok(Some(item)) => {
                    let mut write_buf = self.0.write_buf.borrow_mut();
                    let is_write_sleep = write_buf.is_empty();

                    // encode item
                    if let Err(err) =
                        self.0.codec.borrow_mut().encode(item, &mut write_buf)
                    {
                        log::trace!("Codec encoder error: {:?}", err);
                        flags.insert(Flags::DSP_STOP | Flags::ST_DSP_ERR);
                        self.0.flags.set(flags);
                        self.0.dispatch_task.wake();
                        return Err(Either::Right(err));
                    } else if is_write_sleep {
                        self.0.write_task.wake();
                    }
                    Ok(())
                }
                Err(err) => {
                    flags.insert(Flags::DSP_STOP | Flags::ST_DSP_ERR);
                    self.0.flags.set(flags);
                    self.0.dispatch_task.wake();
                    Err(Either::Left(err))
                }
                _ => Ok(()),
            }
        } else {
            Ok(())
        }
    }
}
