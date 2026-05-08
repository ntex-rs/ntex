use std::cell::{Cell, UnsafeCell};
use std::future::{Future, poll_fn};
use std::task::{Context, Poll};
use std::{fmt, hash, io, marker, mem, ops, pin::Pin, ptr, rc::Rc};

use ntex_bytes::{BytePageSize, BytesMut};
use ntex_codec::{Decoder, Encoder};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_util::{future::Either, task::LocalWaker, time::Sleep};

use crate::buf::Stack;
use crate::cfg::IoConfig;
use crate::ctx::IoContext;
use crate::filter::{Base, Filter, Layer};
use crate::filterptr::FilterPtr;
use crate::flags::Flags;
use crate::seal::{IoBoxed, Sealed};
use crate::timer::{self, Id, TimerHandle};
use crate::{Decoded, FilterLayer, Handle, IoStatusUpdate, IoStream, RecvError};

/// Interface object to underlying io stream
pub struct Io<F = Base>(UnsafeCell<IoRef>, marker::PhantomData<F>);

#[derive(Clone)]
pub struct IoRef(pub(super) Rc<IoState>);

pub(crate) struct IoState {
    filter: FilterPtr,
    pub(super) id: Cell<Id>,
    pub(super) cfg: Cfg<IoConfig>,
    pub(super) flags: Flags,
    pub(super) error: Cell<Option<io::Error>>,
    pub(super) read_task: LocalWaker,
    pub(super) write_task: LocalWaker,
    pub(super) dispatch_task: LocalWaker,
    pub(super) buffer: Stack,
    pub(super) handle: Cell<Option<Box<dyn Handle>>>,
    pub(super) timeout: Cell<TimerHandle>,
    pub(super) shutdown_timeout: Cell<Option<Sleep>>,
    #[allow(clippy::box_collection)]
    pub(super) on_disconnect: Cell<Option<Box<Vec<LocalWaker>>>>,
}

impl IoState {
    pub(super) fn id(&self) -> Id {
        self.id.get()
    }

    pub(super) fn tag(&self) -> &'static str {
        self.cfg.tag()
    }

    pub(super) fn filter(&self) -> &dyn Filter {
        self.filter.get()
    }

    pub(super) fn notify_timeout(&self) {
        if self.flags.check_dispatcher_timeout_unset() {
            self.dispatch_task.wake();
            log::trace!("{}: Timer, notify dispatcher", self.cfg.tag());
        }
    }

    pub(super) fn notify_disconnect(&self) {
        if let Some(on_disconnect) = self.on_disconnect.take() {
            for item in on_disconnect.into_iter() {
                item.wake();
            }
        }
    }

    /// Get current io error
    pub(super) fn error(&self) -> Option<io::Error> {
        if let Some(err) = self.error.take() {
            self.error
                .set(Some(io::Error::new(err.kind(), format!("{err}"))));
            Some(err)
        } else {
            None
        }
    }

    /// Get current io result
    pub(super) fn error_or_disconnected(&self) -> io::Error {
        self.error()
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Disconnected"))
    }

    pub(super) fn filters_stopped(&self) {
        self.read_task.wake();
        self.write_task.wake();
        self.dispatch_task.wake();
        self.flags.set_filters_stopped();
    }

    pub(super) fn terminate_connection(&self, err: Option<io::Error>) {
        if !self.flags.is_stopped() {
            log::trace!(
                "{}: {:?} Io error {:?} flags: {:?}",
                self.cfg.tag(),
                ptr::from_ref(self),
                err,
                self.flags
            );

            if err.is_some() {
                self.error.set(err);
            }
            self.read_task.wake();
            self.write_task.wake();
            self.notify_disconnect();
            self.handle.take();
            self.flags.set_force_closed();

            if !self.dispatch_task.wake_checked() {
                log::trace!(
                    "{}: {:?} Dispatcher is not registered, flags: {:?}",
                    self.cfg.tag(),
                    ptr::from_ref(self),
                    self.flags
                );
            }
        }
    }

    /// Gracefully shutdown read and write io tasks
    pub(super) fn start_shutdown(&self) {
        if !self.flags.is_stopping_any() {
            log::trace!("{}: Initiate io shutdown {:?}", self.cfg.tag(), self.flags);
            self.flags.set_filter_stopping();
            self.read_task.wake();
            self.write_task.wake();
        }
    }

    pub(super) fn get_read_buf(&self) -> BytesMut {
        self.cfg.read_buf().get()
    }

    pub(super) fn enable_rd_backpressure(&self, size: usize) -> bool {
        size >= self.cfg.read_buf().high
    }

    pub(super) fn enable_wr_backpressure(&self, size: usize) -> bool {
        size >= self.cfg.write_buf().high
    }

    pub(super) fn disable_wr_backpressure(&self, size: usize) -> bool {
        size <= self.cfg.write_buf().half
    }
}

impl Eq for IoState {}

impl PartialEq for IoState {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self, other)
    }
}

impl hash::Hash for IoState {
    #[inline]
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        (ptr::from_ref(self) as usize).hash(state);
    }
}

impl fmt::Debug for IoState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let err = self.error.take();
        let res = f
            .debug_struct("IoState")
            .field("id", &self.id)
            .field("flags", &self.flags)
            .field("filter", &self.filter.is_set())
            .field("timeout", &self.timeout)
            .field("error", &err)
            .field("buffer", &self.buffer)
            .field("cfg", &self.cfg)
            .finish();
        self.error.set(err);
        res
    }
}

impl Io {
    #[inline]
    /// Create `Io` instance
    pub fn new<I: IoStream, T: Into<SharedCfg>>(io: I, cfg: T) -> Self {
        let cfg = cfg.into().get::<IoConfig>();
        let size = cfg.write_page_size();
        let flags = Flags::new(cfg.write_buf_limit() > 0);

        let inner = Rc::new(IoState {
            cfg,
            flags,
            id: Cell::new(Id::default()),
            filter: FilterPtr::null(),
            error: Cell::new(None),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            buffer: Stack::new(size),
            handle: Cell::new(None),
            timeout: Cell::new(TimerHandle::default()),
            shutdown_timeout: Cell::new(None),
            on_disconnect: Cell::new(None),
        });
        inner.filter.set(Base::new(IoRef(inner.clone())));

        let ioref = IoRef(inner);
        ioref.0.id.set(timer::register_io(&ioref));

        // start io tasks
        let hnd = io.start(IoContext::new(ioref.clone()));
        if hnd.is_some() {
            ioref.0.flags.set_io_handle_enabled();
        }
        ioref.0.handle.set(hnd);

        Io(UnsafeCell::new(ioref), marker::PhantomData)
    }
}

impl<I: IoStream> From<I> for Io {
    #[inline]
    fn from(io: I) -> Io {
        Io::new(io, SharedCfg::default())
    }
}

impl IoRef {
    fn create_empty() -> IoRef {
        IoRef(Rc::new(IoState {
            id: Cell::new(Id::default()),
            cfg: SharedCfg::default().get::<IoConfig>(),
            filter: FilterPtr::null(),
            flags: Flags::new_stopped(),
            error: Cell::new(None),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            buffer: Stack::new(BytePageSize::Size16),
            handle: Cell::new(None),
            timeout: Cell::new(TimerHandle::default()),
            shutdown_timeout: Cell::new(None),
            on_disconnect: Cell::new(None),
        }))
    }
}

impl<F> Io<F> {
    #[inline]
    /// Get instance of `IoRef`
    pub fn get_ref(&self) -> IoRef {
        self.io_ref().clone()
    }

    #[inline]
    #[must_use]
    /// Clone current io object.
    ///
    /// Current io object becomes closed.
    pub fn take(&self) -> Self {
        Self(UnsafeCell::new(self.take_io_ref()), marker::PhantomData)
    }

    fn take_io_ref(&self) -> IoRef {
        unsafe { mem::replace(&mut *self.0.get(), IoRef::create_empty()) }
    }

    fn st(&self) -> &IoState {
        unsafe { &(*self.0.get()).0 }
    }

    fn io_ref(&self) -> &IoRef {
        unsafe { &*self.0.get() }
    }

    #[inline]
    /// Set shared io config
    pub fn set_config<T: Into<SharedCfg>>(&self, cfg: T) {
        unsafe {
            let cfg = cfg.into().get::<IoConfig>();
            self.st().buffer.set_page_size(cfg.write_page_size());
            self.st().cfg.replace(cfg);
        }
    }
}

impl<F: FilterLayer, T: Filter> Io<Layer<F, T>> {
    #[inline]
    /// Get referece to a filter
    pub fn filter(&self) -> &F {
        &self.st().filter.filter::<Layer<F, T>>().0
    }
}

impl<F: Filter> Io<F> {
    #[inline]
    /// Convert current io stream into sealed version
    pub fn seal(self) -> Io<Sealed> {
        let state = self.take_io_ref();
        state.0.filter.seal::<F>();

        Io(UnsafeCell::new(state), marker::PhantomData)
    }

    #[inline]
    /// Convert current io stream into boxed version
    pub fn boxed(self) -> IoBoxed {
        self.seal().into()
    }

    #[inline]
    /// Add new layer current current filter
    pub fn add_filter<U>(self, nf: U) -> Io<Layer<U, F>>
    where
        U: FilterLayer,
    {
        // write buffer processing could be delayed,
        // need to call filter chain for processing
        if let Err(e) = self.st().buffer.process_write_buf(&self) {
            self.st().terminate_connection(Some(e));
        }

        let state = self.take_io_ref();

        // add buffers layer
        // Safety: .add_layer() only increases internal buffers
        // there is no api that holds references into buffers storage
        // all apis first removes buffer from storage and then work with it
        unsafe { &mut *(Rc::as_ptr(&state.0).cast_mut()) }
            .buffer
            .add_layer();

        // replace current filter
        state.0.filter.add_filter::<F, U>(nf);

        Io(UnsafeCell::new(state), marker::PhantomData)
    }

    /// Map layer
    pub fn map_filter<U, R>(self, f: U) -> Io<R>
    where
        U: FnOnce(F) -> R,
        R: Filter,
    {
        // write buffer processing could be delayed,
        // need to call filter chain for processing
        if let Err(e) = self.st().buffer.process_write_buf(&self) {
            self.st().terminate_connection(Some(e));
        }

        let state = self.take_io_ref();
        state.0.filter.map_filter::<F, U, R>(f);

        Io(UnsafeCell::new(state), marker::PhantomData)
    }
}

impl<F> Io<F> {
    /// Read incoming io stream and decode codec item.
    pub async fn recv<U>(
        &self,
        codec: &U,
    ) -> Result<Option<U::Item>, Either<U::Error, io::Error>>
    where
        U: Decoder,
    {
        loop {
            return match poll_fn(|cx| self.poll_recv(codec, cx)).await {
                Ok(item) => Ok(Some(item)),
                Err(RecvError::KeepAlive) => Err(Either::Right(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timeout",
                ))),
                Err(RecvError::WriteBackpressure) => {
                    poll_fn(|cx| self.poll_flush(cx, false))
                        .await
                        .map_err(Either::Right)?;
                    continue;
                }
                Err(RecvError::Decoder(err)) => Err(Either::Left(err)),
                Err(RecvError::PeerGone(Some(err))) => Err(Either::Right(err)),
                Err(RecvError::PeerGone(None)) => Ok(None),
            };
        }
    }

    /// Pull some bytes from this source into the specified buffer.
    pub async fn read(&self, dst: &mut [u8]) -> io::Result<()> {
        loop {
            let completed = self.with_read_buf(|buf| {
                if buf.len() >= dst.len() {
                    let _ = io::Read::read(buf, dst).expect("Cannot fail");
                    true
                } else {
                    false
                }
            });
            if completed {
                return Ok(());
            }
            self.read_ready().await?;
        }
    }

    #[inline]
    /// Wait until read becomes ready.
    pub async fn read_ready(&self) -> io::Result<Option<()>> {
        poll_fn(|cx| self.poll_read_ready(cx)).await
    }

    #[inline]
    /// Wait until io reads any data.
    pub async fn read_notify(&self) -> io::Result<Option<()>> {
        poll_fn(|cx| self.poll_read_notify(cx)).await
    }

    #[inline]
    /// Pause read task
    pub fn pause(&self) {
        let st = self.st();
        if !st.flags.is_read_paused() {
            st.read_task.wake();
            st.flags.set_read_paused();
        }
    }

    #[inline]
    /// Encode item, send to the peer. Fully flush write buffer.
    pub async fn send<U>(
        &self,
        item: U::Item,
        codec: &U,
    ) -> Result<(), Either<U::Error, io::Error>>
    where
        U: Encoder,
    {
        self.encode(item, codec).map_err(Either::Left)?;

        poll_fn(|cx| self.poll_flush(cx, true))
            .await
            .map_err(Either::Right)?;

        Ok(())
    }

    #[inline]
    /// Wake write task and instruct to flush data.
    ///
    /// This is async version of `poll_flush()` method.
    pub async fn flush(&self, full: bool) -> io::Result<()> {
        poll_fn(|cx| self.poll_flush(cx, full)).await
    }

    #[inline]
    /// Gracefully shutdown io stream
    pub async fn shutdown(&self) -> io::Result<()> {
        poll_fn(|cx| self.poll_shutdown(cx)).await
    }

    #[inline]
    /// Polls for read readiness.
    ///
    /// If the io stream is not currently ready for reading,
    /// this method will store a clone of the Waker from the provided Context.
    /// When the io stream becomes ready for reading, `Waker::wake()` will be called on the waker.
    ///
    /// Return value
    /// The function returns:
    ///
    /// `Poll::Pending` if the io stream is not ready for reading.
    /// `Poll::Ready(Ok(Some(()))))` if the io stream is ready for reading.
    /// `Poll::Ready(Ok(None))` if io stream is disconnected
    /// `Some(Poll::Ready(Err(e)))` if an error is encountered.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<Option<()>>> {
        let st = self.st();

        if st.flags.is_stopped() {
            Poll::Ready(Err(st.error_or_disconnected()))
        } else {
            let ready = st.flags.is_read_ready();

            // If the dispatcher requests more data but no read occurs,
            // restart the read task.
            if st.flags.is_read_full_or_paused() {
                st.flags.unset_all_read_flags();
                st.read_task.wake();
                if ready {
                    Poll::Ready(Ok(Some(())))
                } else {
                    st.dispatch_task.register(cx.waker());
                    Poll::Pending
                }
            } else if ready {
                Poll::Ready(Ok(Some(())))
            } else {
                st.read_task.wake();
                st.dispatch_task.register(cx.waker());
                Poll::Pending
            }
        }
    }

    #[inline]
    /// Polls for any incoming data.
    pub fn poll_read_notify(&self, cx: &mut Context<'_>) -> Poll<io::Result<Option<()>>> {
        let ready = self.poll_read_ready(cx);

        if ready.is_pending() {
            let st = self.st();
            if st.flags.check_read_notify() {
                Poll::Ready(Ok(Some(())))
            } else {
                st.flags.set_read_notify();
                Poll::Pending
            }
        } else {
            ready
        }
    }

    #[inline]
    /// Decode codec item from incoming bytes stream.
    ///
    /// Wake read task and request to read more data if data is not enough for decoding.
    /// If error get returned this method does not register waker for later wake up action.
    pub fn poll_recv<U>(
        &self,
        codec: &U,
        cx: &mut Context<'_>,
    ) -> Poll<Result<U::Item, RecvError<U>>>
    where
        U: Decoder,
    {
        let decoded = self.poll_recv_decode(codec, cx)?;

        if let Some(item) = decoded.item {
            Poll::Ready(Ok(item))
        } else {
            Poll::Pending
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Decode codec item from incoming bytes stream.
    ///
    /// Wake read task and request to read more data if data is not enough for decoding.
    /// If error get returned this method does not register waker for later wake up action.
    pub fn poll_recv_decode<U>(
        &self,
        codec: &U,
        cx: &mut Context<'_>,
    ) -> Result<Decoded<U::Item>, RecvError<U>>
    where
        U: Decoder,
    {
        self.st().flags.unset_read_ready();

        let decoded = self
            .decode_item(codec)
            .map_err(|err| RecvError::Decoder(err))?;

        if decoded.item.is_some() {
            Ok(decoded)
        } else {
            let st = self.st();
            if st.flags.is_stopped() {
                Err(RecvError::PeerGone(st.error()))
            } else if st.flags.check_dispatcher_timeout() {
                Err(RecvError::KeepAlive)
            } else if st.flags.is_wr_backpressure() {
                Err(RecvError::WriteBackpressure)
            } else {
                match self.poll_read_ready(cx) {
                    Poll::Pending | Poll::Ready(Ok(Some(()))) => {
                        if log::log_enabled!(log::Level::Trace) && decoded.remains != 0 {
                            log::trace!(
                                "{}: Not enough data to decode next frame",
                                self.tag()
                            );
                        }
                        Ok(decoded)
                    }
                    Poll::Ready(Err(e)) => Err(RecvError::PeerGone(Some(e))),
                    Poll::Ready(Ok(None)) => Err(RecvError::PeerGone(None)),
                }
            }
        }
    }

    #[inline]
    /// Wake write task and instruct to flush data.
    ///
    /// If `full` is true then wake up dispatcher when all data is flushed
    /// otherwise wake up when size of write buffer is lower than
    /// buffer max size.
    pub fn poll_flush(&self, cx: &mut Context<'_>, full: bool) -> Poll<io::Result<()>> {
        let st = self.st();
        st.buffer.process_write_buf_force(self)?;

        let len = st.buffer.write_buffer_size();
        if len > 0 {
            if full {
                return if st.flags.is_stopped() {
                    Poll::Ready(Err(st.error_or_disconnected()))
                } else {
                    st.dispatch_task.register(cx.waker());
                    st.flags.set_wants_write_flush();
                    Poll::Pending
                };
            } else if st.enable_wr_backpressure(len) {
                return if st.flags.is_stopped() {
                    Poll::Ready(Err(st.error_or_disconnected()))
                } else {
                    st.flags.set_wr_backpressure();
                    st.dispatch_task.register(cx.waker());
                    Poll::Pending
                };
            }
        }
        if st.flags.is_stopped() {
            Poll::Ready(Err(st.error_or_disconnected()))
        } else {
            st.flags.unset_wr_backpressure();
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    /// Gracefully shutdown io stream
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let st = self.st();

        if st.flags.is_stopped() {
            if let Some(err) = st.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(()))
            }
        } else {
            if !st.flags.is_stopping_filters() {
                st.start_shutdown();
            }

            st.read_task.wake();
            st.write_task.wake();
            st.dispatch_task.register(cx.waker());
            Poll::Pending
        }
    }

    #[inline]
    /// Pause read task
    ///
    /// Returns status updates
    pub fn poll_read_pause(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        self.pause();
        self.poll_status_update(cx)
    }

    #[inline]
    /// Wait for status updates
    pub fn poll_status_update(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        let st = self.st();
        st.dispatch_task.register(cx.waker());
        if st.flags.is_closed() {
            Poll::Ready(IoStatusUpdate::PeerGone(st.error()))
        } else if st.flags.check_dispatcher_timeout() {
            Poll::Ready(IoStatusUpdate::KeepAlive)
        } else if st.flags.is_wr_backpressure() {
            // write backpressure is enabled and write buf smaller than half
            if st.disable_wr_backpressure(st.buffer.write_buffer_size()) {
                st.flags.unset_wr_backpressure();
            }
            Poll::Ready(IoStatusUpdate::WriteBackpressure)
        } else {
            Poll::Pending
        }
    }

    #[inline]
    /// Register dispatch task
    pub fn poll_dispatch(&self, cx: &mut Context<'_>) {
        self.st().dispatch_task.register(cx.waker());
    }
}

impl<F> AsRef<IoRef> for Io<F> {
    #[inline]
    fn as_ref(&self) -> &IoRef {
        self.io_ref()
    }
}

impl<F> Eq for Io<F> {}

impl<F> PartialEq for Io<F> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.io_ref().eq(other.io_ref())
    }
}

impl<F> hash::Hash for Io<F> {
    #[inline]
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.io_ref().hash(state);
    }
}

impl<F> fmt::Debug for Io<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Io").field("state", self.st()).finish()
    }
}

impl<F> ops::Deref for Io<F> {
    type Target = IoRef;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.io_ref()
    }
}

impl<F> Drop for Io<F> {
    fn drop(&mut self) {
        timer::unregister_io(self.io_ref());

        let st = self.st();
        self.stop_timer();

        if st.filter.is_set() {
            // filter is unsafe and must be dropped explicitly,
            // and won't be dropped without special attention
            if !st.flags.is_stopped() {
                log::trace!(
                    "{}: Io is dropped, force stopping io streams {:?}",
                    st.cfg.tag(),
                    st.flags
                );
            }

            st.terminate_connection(None);
            st.filter.drop_filter::<F>();
        }
    }
}

#[derive(Debug)]
/// `OnDisconnect` future resolves when socket get disconnected
#[must_use = "OnDisconnect do nothing unless polled"]
pub struct OnDisconnect {
    token: usize,
    inner: Rc<IoState>,
}

impl OnDisconnect {
    pub(super) fn new(inner: Rc<IoState>) -> Self {
        Self::new_inner(inner.flags.is_stopped(), inner)
    }

    fn new_inner(disconnected: bool, inner: Rc<IoState>) -> Self {
        let token = if disconnected {
            usize::MAX
        } else {
            let mut on_disconnect = inner.on_disconnect.take();
            let token = if let Some(ref mut on_disconnect) = on_disconnect {
                let token = on_disconnect.len();
                on_disconnect.push(LocalWaker::default());
                token
            } else {
                on_disconnect = Some(Box::new(vec![LocalWaker::default()]));
                0
            };
            inner.on_disconnect.set(on_disconnect);
            token
        };
        Self { token, inner }
    }

    #[inline]
    /// Check if connection is disconnected
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.token == usize::MAX || self.inner.flags.is_stopped() {
            Poll::Ready(())
        } else if let Some(on_disconnect) = self.inner.on_disconnect.take() {
            on_disconnect[self.token].register(cx.waker());
            self.inner.on_disconnect.set(Some(on_disconnect));
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

impl Clone for OnDisconnect {
    fn clone(&self) -> Self {
        if self.token == usize::MAX {
            OnDisconnect::new_inner(true, self.inner.clone())
        } else {
            OnDisconnect::new_inner(false, self.inner.clone())
        }
    }
}

impl Future for OnDisconnect {
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_ready(cx)
    }
}

#[cfg(test)]
mod tests {
    use ntex_bytes::{Bytes, BytesMut};
    use ntex_codec::BytesCodec;
    use ntex_util::{future::lazy, time::Millis, time::sleep};

    use super::*;
    use crate::testing::IoTest;

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";
    const BIN2: &[u8] = b"12345678901234561234567890123456";

    #[ntex::test]
    async fn test_basics() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::from(server);
        assert!(server.eq(&server));
        assert!(server.io_ref().eq(server.io_ref()));
    }

    #[ntex::test]
    async fn test_recv() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::new(server, SharedCfg::new("SRV"));

        server.st().notify_timeout();
        let err = server.recv(&BytesCodec).await.err().unwrap();
        assert!(format!("{err:?}").contains("Timeout"));

        client.write(TEXT);
        server.st().flags.set_wr_backpressure();
        let item = server.recv(&BytesCodec).await.ok().unwrap().unwrap();
        assert_eq!(item, TEXT);
    }

    #[ntex::test]
    async fn test_send() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::from(server);
        assert!(server.eq(&server));

        server
            .send(Bytes::from_static(BIN), &BytesCodec)
            .await
            .ok()
            .unwrap();
        let item = client.read_any();
        assert_eq!(item, TEXT);
    }

    #[ntex::test]
    async fn read_readiness() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let io = Io::from(server);
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());

        client.write(TEXT);
        assert_eq!(io.read_ready().await.unwrap(), Some(()));
        assert!(matches!(
            lazy(|cx| io.poll_read_ready(cx)).await,
            Poll::Ready(Ok(Some(())))
        ));

        let item = io.with_read_buf(BytesMut::take);
        assert_eq!(item, Bytes::from_static(BIN));

        client.write(TEXT);
        sleep(Millis(50)).await;
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_ready());
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_ready());
    }

    #[ntex::test]
    async fn read_backpressure() {
        let (client, server) = IoTest::create();

        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(IoConfig::default().set_read_buf(64, 32, 12)),
        );
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());

        client.write(BIN2);
        client.write(BIN2);
        sleep(Millis(50)).await;
        assert!(io.flags().is_read_ready());
        assert!(io.flags().is_rd_backpressure());
        let _item = io.recv(&BytesCodec).await.ok().unwrap().unwrap();
        assert!(!io.flags().is_read_ready());
        assert!(!io.flags().is_rd_backpressure());

        client.write(BIN2);
        client.write(BIN2);
        sleep(Millis(50)).await;
        assert!(io.flags().is_read_ready());
        assert!(io.flags().is_rd_backpressure());
        assert_eq!(io.read_ready().await.unwrap(), Some(()));
    }

    #[ntex::test]
    async fn write_backpressure() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);

        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_buf(16, 8, 12)),
        );
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());
        assert!(io.flags().is_write_paused());
        assert!(!io.flags().is_wr_backpressure());
        assert!(!io.is_wr_backpressure());

        io.encode_slice(BIN2).unwrap();
        assert!(!io.flags().is_write_paused());
        assert!(io.flags().is_wr_backpressure());

        client.remote_buffer_cap(1024);
        let item = client.read().await.unwrap();
        assert_eq!(item, BIN2);
        assert!(io.flags().is_wr_backpressure());
        assert!(matches!(
            lazy(|cx| io.poll_status_update(cx)).await,
            Poll::Ready(IoStatusUpdate::WriteBackpressure)
        ));
        assert!(!io.flags().is_wr_backpressure());
        assert!(matches!(
            lazy(|cx| io.poll_flush(cx, false)).await,
            Poll::Ready(Ok(()))
        ));
        assert!(!io.flags().is_wr_backpressure());
    }
}
