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
use crate::ops::{Id, IoManager, TimerHandle};
use crate::seal::{IoBoxed, Sealed};
use crate::{Decoded, FilterLayer, Handle, IoStatusUpdate, IoStream, RecvError};

/// Interface object to the underlying I/O stream
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
    dispatch_task: LocalWaker,
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
            self.wake_dispatch_task();
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

    /// Get the current I/O error.
    pub(super) fn error(&self) -> Option<io::Error> {
        if let Some(err) = self.error.take() {
            self.error
                .set(Some(io::Error::new(err.kind(), format!("{err}"))));
            Some(err)
        } else {
            None
        }
    }

    /// Returns the current I/O error, or creates a `NotConnected` error.
    pub(super) fn error_or_disconnected(&self) -> io::Error {
        self.error()
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Disconnected"))
    }

    pub(super) fn filters_stopped(&self) {
        self.wake_read_task();
        self.wake_write_task();
        self.wake_dispatch_task();
        self.flags.set_filters_stopped();
    }

    pub(super) fn terminate_connection(&self, err: Option<io::Error>) {
        if !self.flags.is_terminated() {
            log::trace!("{}: Terminate io with error {:?}", self.cfg.tag(), err);
            if err.is_some() {
                self.error.set(err);
            }
            self.flags.set_terminate();
            self.wake_read_task();
            self.wake_write_task();
            self.wake_dispatch_task();
            self.notify_disconnect();
            self.handle.take();
        }
    }

    /// Gracefully shuts down the read and write I/O tasks.
    pub(super) fn start_shutdown(&self) {
        if !self.flags.is_stopping_any() {
            log::trace!("{}: Initiate io shutdown {:?}", self.cfg.tag(), self.flags);
            self.flags.set_filter_stopping();
            self.wake_read_task();
            self.wake_write_task();
        }
    }

    pub(super) fn get_read_buf(&self) -> BytesMut {
        self.cfg.read_buf().get()
    }

    pub(super) fn is_rd_backpressure_needed(&self, size: usize) -> bool {
        size >= self.cfg.read_buf().high
    }

    pub(super) fn is_wr_backpressure_needed(&self, size: usize) -> bool {
        size >= self.cfg.write_buf().high
    }

    pub(super) fn should_disable_wr_backpressure(&self, size: usize) -> bool {
        size <= self.cfg.write_buf().half
    }

    pub(super) fn wake_read_task(&self) {
        self.read_task.wake();
    }

    pub(super) fn wake_write_task(&self) {
        self.write_task.wake();
    }

    pub(super) fn wake_dispatch_task(&self) {
        self.dispatch_task.wake();
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
    /// Creates a new `Io` instance.
    pub fn new<I: IoStream, T: Into<SharedCfg>>(io: I, cfg: T) -> Self {
        let cfg = cfg.into().get::<IoConfig>();
        let size = cfg.write_page_size();
        let flags = Flags::new(cfg.write_buf_threshold() > 0);

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
        ioref.0.id.set(IoManager::register(&ioref));

        // start io tasks
        let hnd = io.start(IoContext::new(ioref.clone()));
        ioref.0.handle.set(Some(hnd));

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
    /// Get an instance of `IoRef`.
    pub fn get_ref(&self) -> IoRef {
        self.io_ref().clone()
    }

    #[inline]
    #[must_use]
    /// Takes the current I/O object.
    ///
    /// After this call, the I/O object is no longer valid for use.
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
    /// Updates the shared I/O configuration.
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
    /// Returns a reference to a filter.
    pub fn filter(&self) -> &F {
        &self.st().filter.filter::<Layer<F, T>>().0
    }
}

impl<F: Filter> Io<F> {
    #[inline]
    /// Convert the current I/O stream into a sealed version.
    pub fn seal(self) -> Io<Sealed> {
        let state = self.take_io_ref();
        state.0.filter.seal::<F>();

        Io(UnsafeCell::new(state), marker::PhantomData)
    }

    #[inline]
    /// Convert the current I/O stream into a boxed version.
    pub fn boxed(self) -> IoBoxed {
        self.seal().into()
    }

    #[inline]
    /// Adds a new processing layer to the current filter chain.
    pub fn add_filter<U>(self, nf: U) -> Io<Layer<U, F>>
    where
        U: FilterLayer,
    {
        // Write buffer processing may be delayed,
        // call the filter chain to process pending writes
        if let Err(e) = self.st().buffer.process_write_buf(&self) {
            self.st().terminate_connection(Some(e));
        }

        let state = self.take_io_ref();

        // Add the buffers layer.
        //
        // Safety: no references into the buffer storage are retained.
        // All APIs first remove the buffer from storage before processing it.
        unsafe { &mut *(Rc::as_ptr(&state.0).cast_mut()) }
            .buffer
            .add_layer();

        // Replace current filter
        state.0.filter.add_filter::<F, U>(nf);

        if let Err(e) = self.st().buffer.process_read_buf(&self, BytesMut::new()) {
            self.st().terminate_connection(Some(e));
        }

        Io(UnsafeCell::new(state), marker::PhantomData)
    }

    /// Wraps the current layer with a wrapper.
    pub fn map_filter<U, R>(self, f: U) -> Io<R>
    where
        U: FnOnce(F) -> R,
        R: Filter,
    {
        // Write buffer processing may be delayed,
        // call the filter chain to process pending writes
        if let Err(e) = self.st().buffer.process_write_buf(&self) {
            self.st().terminate_connection(Some(e));
        }

        let state = self.take_io_ref();
        state.0.filter.map_filter::<F, U, R>(f);

        Io(UnsafeCell::new(state), marker::PhantomData)
    }
}

impl<F> Io<F> {
    /// Reads from the incoming I/O stream and decodes a codec item.
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

    /// Reads bytes from this I/O stream into the specified buffer.
    ///
    /// If there is not enough data available, waits for incoming data.
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
    /// Waits until the I/O stream is ready for reading.
    pub async fn read_ready(&self) -> io::Result<Option<()>> {
        poll_fn(|cx| self.poll_read_ready(cx)).await
    }

    #[inline]
    /// Waits until the I/O stream receives new data.
    pub async fn read_notify(&self) -> io::Result<Option<()>> {
        poll_fn(|cx| self.poll_read_notify(cx)).await
    }

    #[inline]
    /// Pauses the read task.
    pub fn pause(&self) {
        let st = self.st();
        if !st.flags.is_read_paused() {
            st.wake_read_task();
            st.flags.set_read_paused();
        }
    }

    #[inline]
    /// Encodes an item and sends it to the peer, fully flushing the write buffer.
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
    /// Wakes the write task and requests a flush of buffered data.
    ///
    /// This is the async version of `.poll_flush()` method.
    pub async fn flush(&self, full: bool) -> io::Result<()> {
        poll_fn(|cx| self.poll_flush(cx, full)).await
    }

    #[inline]
    /// Gracefully shuts down the I/O stream.
    pub async fn shutdown(&self) -> io::Result<()> {
        poll_fn(|cx| self.poll_shutdown(cx)).await
    }

    #[inline]
    /// Polls for read readiness.
    ///
    /// If the I/O stream is not currently ready for reading,
    /// this method will store a clone of the `Waker` from the provided `Context`.
    /// When the I/O stream becomes ready for reading, `wake()` will be called on the waker.
    ///
    /// # Returns
    ///
    /// The function returns:
    ///
    /// - `Poll::Pending` if the I/O stream is not ready for reading.
    /// - `Poll::Ready(Ok(Some(())))` if the I/O stream is ready for reading.
    /// - `Poll::Ready(Ok(None))` if the I/O stream is disconnected.
    /// - `Poll::Ready(Err(e))` if an error is encountered.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<Option<()>>> {
        let st = self.st();

        if st.flags.is_closed() {
            Poll::Ready(Ok(None))
        } else {
            let ready = st.flags.is_read_ready();

            // If the dispatcher requests more data but no read occurs,
            // restart the read task.
            if st.flags.is_read_paused_or_backpressure() {
                st.flags.unset_all_read_flags();
                st.flags.unset_read_paused();
                st.wake_read_task();
                if ready {
                    Poll::Ready(Ok(Some(())))
                } else {
                    st.dispatch_task.register(cx.waker());
                    Poll::Pending
                }
            } else if ready {
                Poll::Ready(Ok(Some(())))
            } else {
                if st.flags.is_read_paused() {
                    st.wake_read_task();
                    st.flags.unset_read_paused();
                }
                st.dispatch_task.register(cx.waker());
                Poll::Pending
            }
        }
    }

    #[inline]
    /// Polls the I/O stream for availability of incoming data.
    pub fn poll_read_notify(&self, cx: &mut Context<'_>) -> Poll<io::Result<Option<()>>> {
        let st = self.st();
        if st.flags.is_stopping() {
            Poll::Ready(Ok(None))
        } else if st.flags.check_read_notifed() {
            Poll::Ready(Ok(Some(())))
        } else {
            st.flags.set_read_notify();
            if self.poll_read_ready(cx).is_ready() {
                st.dispatch_task.register(cx.waker());
            }
            st.dispatch_task.register(cx.waker());
            Poll::Pending
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
        let st = self.st();
        st.flags.unset_read_ready();

        let decoded = self
            .decode_item(codec)
            .map_err(|err| RecvError::Decoder(err))?;

        if decoded.item.is_some() {
            Ok(decoded)
        } else if st.flags.is_stopping() {
            Err(RecvError::PeerGone(st.error()))
        } else if st.flags.check_dispatcher_timeout() {
            Err(RecvError::KeepAlive)
        } else if st.flags.is_wr_backpressure() {
            Err(RecvError::WriteBackpressure)
        } else {
            match self.poll_read_ready(cx) {
                Poll::Pending | Poll::Ready(Ok(Some(()))) => {
                    #[cfg(feature = "trace")]
                    if decoded.remains != 0 {
                        log::trace!("{}: Not enough data to decode next frame", self.tag());
                    }
                    Ok(decoded)
                }
                Poll::Ready(Err(e)) => Err(RecvError::PeerGone(Some(e))),
                Poll::Ready(Ok(None)) => Err(RecvError::PeerGone(None)),
            }
        }
    }

    #[inline]
    /// Wakes the write task and instructs it to flush data.
    ///
    /// If `full` is true, wakes the dispatcher when all data has been flushed;
    /// otherwise, it wakes when the write buffer size falls below the low-watermark size.
    pub fn poll_flush(&self, cx: &mut Context<'_>, full: bool) -> Poll<io::Result<()>> {
        let st = self.st();
        st.buffer.process_write_buf_force(self)?;
        self.update_write_destination();

        let len = st.buffer.write_buf_size();
        if len > 0 {
            if st.flags.is_closed() {
                return Poll::Ready(Err(st.error_or_disconnected()));
            } else if full {
                st.dispatch_task.register(cx.waker());
                st.flags.set_wants_write_flush();
                return Poll::Pending;
            } else if st.is_wr_backpressure_needed(len) {
                st.flags.set_wr_backpressure();
                st.dispatch_task.register(cx.waker());
                return Poll::Pending;
            }
        }
        if st.flags.is_closed() && !st.flags.is_write_flush() {
            Poll::Ready(Err(st.error_or_disconnected()))
        } else {
            st.flags.unset_wr_backpressure_and_flush();
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    /// Gracefully shuts down the I/O stream.
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let st = self.st();

        if st.flags.is_stopping() {
            if let Some(err) = st.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(()))
            }
        } else {
            if !st.flags.is_stopping_filters() {
                st.start_shutdown();
            }
            st.flags.unset_all_read_flags();
            st.flags.unset_read_paused();

            st.wake_read_task();
            st.dispatch_task.register(cx.waker());
            Poll::Pending
        }
    }

    #[inline]
    /// Pauses the read task.
    ///
    /// Returns status updates.
    pub fn poll_read_pause(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        self.pause();
        self.poll_status_update(cx)
    }

    #[inline]
    /// Polls for available status updates.
    pub fn poll_status_update(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        let st = self.st();
        st.dispatch_task.register(cx.waker());
        if st.flags.is_closed() {
            Poll::Ready(IoStatusUpdate::PeerGone(st.error()))
        } else if st.flags.check_dispatcher_timeout() {
            Poll::Ready(IoStatusUpdate::KeepAlive)
        } else if st.flags.is_wr_backpressure() {
            // write backpressure is enabled and write buf smaller than half
            if st.should_disable_wr_backpressure(st.buffer.write_buf_size()) {
                st.flags.unset_wr_backpressure();
            }
            Poll::Ready(IoStatusUpdate::WriteBackpressure)
        } else {
            Poll::Pending
        }
    }

    #[inline]
    /// Registers a dispatch task.
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
        let st = self.st();
        self.stop_timer();

        if st.filter.is_set() {
            // filter is unsafe and must be dropped explicitly,
            // and won't be dropped without special attention
            if !st.flags.is_terminated() {
                log::trace!("{}: Io is dropped, terminate connection", st.tag());
            }

            st.terminate_connection(None);
            st.filter.drop_filter::<F>();
        }

        IoManager::unregister(self.io_ref());
    }
}

#[derive(Debug)]
/// The `OnDisconnect` future resolves when the I/O stream is disconnected.
#[must_use = "OnDisconnect do nothing unless polled"]
pub struct OnDisconnect {
    token: usize,
    inner: Rc<IoState>,
}

impl OnDisconnect {
    pub(super) fn new(inner: Rc<IoState>) -> Self {
        Self::new_inner(inner.flags.is_stopping(), inner)
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
    /// Checks if the I/O stream is disconnected.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.token == usize::MAX || self.inner.flags.is_stopping() {
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
    use ntex_bytes::{BufMut, BytePages, Bytes, BytesMut};
    use ntex_codec::BytesCodec;
    use ntex_util::{future::lazy, time::Millis, time::sleep};

    use super::*;
    use crate::{
        FilterBuf, IoContext, IoTaskStatus, Readiness, ops::Iops, testing::IoTest,
    };

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
    async fn read() {
        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default().set_read_buf(8, 4, 16)),
        );
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());
        assert!(io.st().dispatch_task.is_set());

        let ctx = IoContext::new(io.get_ref());

        // Ready
        assert_eq!(
            lazy(|cx| ctx.poll_read_ready(cx)).await,
            Poll::Ready(Readiness::Ready)
        );
        assert!(io.st().read_task.is_set());
        assert!(!io.st().flags.is_read_ready());
        assert!(!io.st().flags.is_rd_backpressure());

        // == Enable backpressure
        let buf = BytesMut::copy_from_slice(b"1234567890");
        ctx.update_read_status(Ok(Some(buf)));

        // dispatcher is woken
        assert!(!io.st().dispatch_task.is_set());
        // read task is paused
        assert!(io.st().flags.is_read_paused());
        // read buffer is ready
        assert!(io.st().flags.is_read_ready());
        // read backpressure is enabled
        assert!(io.st().flags.is_rd_backpressure());
        // read task paused
        assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);

        // read one byte
        assert_eq!(io.with_read_buf(|buf| buf.split_to(1)), b"1");
        // read buffer is ready
        assert!(io.st().flags.is_read_ready());
        // read backpressure is enabled
        assert!(io.st().flags.is_rd_backpressure());

        // read task is set
        assert!(io.st().read_task.is_set());

        // read one more byte
        assert_eq!(io.with_read_buf(|buf| buf.split_to(1)), b"2");
        // read backpressure is enabled
        assert!(io.st().flags.is_rd_backpressure());

        // read 4 bytes. buf size is 4, less that half of high watermark
        assert_eq!(io.with_read_buf(|buf| buf.split_to(4)), b"3456");
        // read task is not paused anymore
        assert!(!io.st().flags.is_read_paused());
        // read buffer is not ready
        assert!(!io.st().flags.is_read_ready());
        // read backpressure is disabled
        assert!(!io.st().flags.is_rd_backpressure());
        // read task is woken
        assert!(!io.st().read_task.is_set());
        assert_eq!(
            lazy(|cx| ctx.poll_read_ready(cx)).await,
            Poll::Ready(Readiness::Ready)
        );

        // register dispatcher task
        lazy(|cx| io.poll_dispatch(cx)).await;

        // == Enable backpressure, 4 bytes in buffer + 4 more
        let buf = BytesMut::copy_from_slice(b"1234");
        ctx.update_read_status(Ok(Some(buf)));

        // dispatcher is woken
        assert!(!io.st().dispatch_task.is_set());
        // read task is paused
        assert!(io.st().flags.is_read_paused());
        // read buffer is ready
        assert!(io.st().flags.is_read_ready());
        // read backpressure is enabled
        assert!(io.st().flags.is_rd_backpressure());
        // read task paused
        assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);

        // read 4 bytes. buf size is 4, less that half of high watermark
        assert_eq!(io.with_read_buf(|buf| buf.split_to(4)), b"7890");
        // read backpressure is disabled
        assert!(!io.st().flags.is_rd_backpressure());

        // register dispatcher task
        lazy(|cx| io.poll_dispatch(cx)).await;

        // == No backpressure, 4 bytes in buffer + 3 more
        let buf = BytesMut::copy_from_slice(b"567");
        ctx.update_read_status(Ok(Some(buf)));

        // read task is paused
        assert!(!io.st().flags.is_read_paused());
        // read buffer is ready
        assert!(io.st().flags.is_read_ready());
        // read backpressure is enabled
        assert!(!io.st().flags.is_rd_backpressure());
        // read task ready
        assert_eq!(
            lazy(|cx| ctx.poll_read_ready(cx)).await,
            Poll::Ready(Readiness::Ready)
        );

        // read 4 bytes. buf size is 4, less that half of high watermark
        assert_eq!(io.with_read_buf(BytesMut::take), b"1234567");
        // read task is paused
        assert!(!io.st().flags.is_read_paused());
        // read buffer is ready
        assert!(!io.st().flags.is_read_ready());
        // read task is not woken
        assert!(io.st().read_task.is_set());

        // == Terminate
        io.terminate();
        // read task is woken
        assert!(!io.st().read_task.is_set());
        // read task ready
        assert_eq!(
            lazy(|cx| ctx.poll_read_ready(cx)).await,
            Poll::Ready(Readiness::Terminate)
        );
    }

    #[ntex::test]
    async fn read_notify() {
        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default().set_read_buf(8, 4, 16)),
        );
        assert!(!io.st().flags.is_read_notify());
        assert!(lazy(|cx| io.poll_read_notify(cx)).await.is_pending());
        assert!(io.st().dispatch_task.is_set());
        assert!(io.st().flags.is_read_notify());

        let ctx = IoContext::new(io.get_ref());

        // incoming bytes
        ctx.update_read_status(Ok(Some(BytesMut::copy_from_slice(b"1"))));

        assert!(!io.st().dispatch_task.is_set());
        // rd buffer is ready
        assert!(io.st().flags.is_read_ready());
        assert!(io.st().flags.is_read_notify());
        // dispatcher is notified
        assert!(io.st().flags.is_read_notified());
        let res = lazy(|cx| io.poll_read_notify(cx)).await;
        assert!(matches!(res, Poll::Ready(Ok(Some(())))));

        // disapcher is not set
        assert!(!io.st().dispatch_task.is_set());
        // rd buffer is ready
        assert!(io.st().flags.is_read_ready());

        // == start notification process again
        assert!(lazy(|cx| io.poll_read_notify(cx)).await.is_pending());
        assert!(io.st().dispatch_task.is_set());
        assert!(io.st().flags.is_read_notify());
        assert!(io.st().flags.is_read_ready());
        // read task ready
        assert_eq!(
            lazy(|cx| ctx.poll_read_ready(cx)).await,
            Poll::Ready(Readiness::Ready)
        );

        // == enable packpressure
        ctx.update_read_status(Ok(Some(BytesMut::copy_from_slice(b"2345678"))));
        // read backpressure is enabled
        assert!(io.st().flags.is_rd_backpressure());

        // rd buffer is ready
        assert!(io.st().flags.is_read_ready());
        assert!(io.st().flags.is_read_notify());
        // dispatcher is notified
        assert!(io.st().flags.is_read_notified());
        let res = lazy(|cx| io.poll_read_notify(cx)).await;
        assert!(matches!(res, Poll::Ready(Ok(Some(())))));
        // read task paused
        assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);
        // read task is set
        assert!(io.st().read_task.is_set());

        // == start notification process again
        assert!(lazy(|cx| io.poll_read_notify(cx)).await.is_pending());
        // read flags active
        assert!(!io.st().flags.is_rd_backpressure());
        assert!(!io.st().flags.is_read_ready());
        assert!(!io.st().flags.is_read_paused());
        // read task is woken
        assert!(!io.st().read_task.is_set());
        // read task ready
        assert_eq!(
            lazy(|cx| ctx.poll_read_ready(cx)).await,
            Poll::Ready(Readiness::Ready)
        );

        // incoming bytes
        ctx.update_read_status(Ok(Some(BytesMut::copy_from_slice(b"1"))));
        assert!(!io.st().dispatch_task.is_set());
        // rd buffer is ready
        assert!(io.st().flags.is_read_ready());
        assert!(io.st().flags.is_read_notify());
        assert!(io.st().flags.is_read_paused());
        assert!(io.st().flags.is_rd_backpressure());
        // dispatcher is notified
        assert!(io.st().flags.is_read_notified());
        assert!(matches!(
            lazy(|cx| io.poll_read_notify(cx)).await,
            Poll::Ready(Ok(Some(())))
        ));

        // == Terminate
        io.terminate();
        let res = lazy(|cx| io.poll_read_notify(cx)).await;
        assert!(matches!(res, Poll::Ready(Ok(None))), "{res:?}");
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
    async fn write() {
        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_buf(8, 4, 16)),
        );
        assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
        assert!(io.st().dispatch_task.is_set());
        assert!(io.st().flags.is_direct_wr_enabled());

        let ctx = IoContext::new(io.get_ref());

        // == No write work
        assert_eq!(lazy(|cx| ctx.poll_write_ready(cx)).await, Poll::Pending);
        assert!(io.st().write_task.is_set());
        assert!(io.st().flags.is_write_paused());
        assert!(!io.st().flags.is_wr_backpressure());

        // write
        io.with_write_buf(|buf| buf.put_slice(b"1234")).unwrap();
        assert_eq!(lazy(|cx| ctx.poll_write_ready(cx)).await, Poll::Pending);
        // write task is paused
        assert!(io.st().flags.is_write_paused());
        // send-buf op is scheduled
        assert!(io.st().flags.is_wr_send_scheduled());
        // back-pressure is not enabled
        assert!(!io.st().flags.is_wr_backpressure());
        // dispatch is not woken up
        assert!(io.st().dispatch_task.is_set());

        // == enable wr backpressure
        io.with_write_buf(|buf| buf.put_slice(b"5678")).unwrap();
        // back-pressure is enabled
        assert!(io.st().flags.is_wr_backpressure());
        // dispatch is woken up
        assert!(!io.st().dispatch_task.is_set());
        // write task is set
        assert!(io.st().write_task.is_set());
        // dispatcher gets WriteBackpressure
        assert!(matches!(
            lazy(|cx| io.poll_status_update(cx)).await,
            Poll::Ready(IoStatusUpdate::WriteBackpressure)
        ));
        // flush write buffer
        assert!(lazy(|cx| io.poll_flush(cx, false)).await.is_pending());
        // full flush is not enabled
        assert!(!io.st().flags.is_write_flush());

        // run send-buf ops
        Iops::run();
        // send-buf op is not scheduled
        assert!(!io.st().flags.is_wr_send_scheduled());
        // write task is not paused
        assert!(!io.st().flags.is_write_paused());
        // write task has been woken up
        assert!(!io.st().write_task.is_set());
        // write task can proceed
        assert_eq!(
            lazy(|cx| ctx.poll_write_ready(cx)).await,
            Poll::Ready(Readiness::Ready)
        );

        // wrote 4 bytes to io
        assert_eq!(ctx.with_write_buf(|buf| buf.split_to(4).freeze()), b"1234");
        // continue to write
        assert_eq!(ctx.update_write_status(Ok(true)), IoTaskStatus::Io);
        // write task can proceed
        assert_eq!(
            lazy(|cx| ctx.poll_write_ready(cx)).await,
            Poll::Ready(Readiness::Ready)
        );
        // write task is not paused
        assert!(!io.st().flags.is_write_paused());
        // back-pressure is enabled
        assert!(io.st().flags.is_wr_backpressure());
        // dispatcher gets WriteBackpressure, buf wr-backpressure flags is removed
        assert!(matches!(
            lazy(|cx| io.poll_status_update(cx)).await,
            Poll::Ready(IoStatusUpdate::WriteBackpressure)
        ));
        // back-pressure is disabled
        assert!(!io.st().flags.is_wr_backpressure());
        assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
        // write buffer is flushed
        assert!(matches!(
            lazy(|cx| io.poll_flush(cx, false)).await,
            Poll::Ready(Ok(()))
        ));

        // full flush write buffer
        io.with_write_buf(|buf| buf.put_slice(b"1234")).unwrap();
        assert!(lazy(|cx| io.poll_flush(cx, true)).await.is_pending());
        // full flush is enabled
        assert!(io.st().flags.is_write_flush());
        // back-pressure is enabled
        assert!(io.st().flags.is_wr_backpressure());

        // wrote all data
        Iops::run();
        assert_eq!(ctx.with_write_buf(BytePages::freeze), b"56781234");
        // write task is not paused, so send-buf op is not scheduled
        assert!(!io.st().flags.is_wr_send_scheduled());
        // update status, no more work
        assert_eq!(ctx.update_write_status(Ok(true)), IoTaskStatus::Pause);
        // write task is paused
        assert!(io.st().flags.is_write_paused());
        // flush is still enabled
        assert!(io.st().flags.is_write_flush());
        // back-pressure is still enabled
        assert!(io.st().flags.is_wr_backpressure());
        // dispatch is woken up
        assert!(!io.st().dispatch_task.is_set());

        // write buffer is flushed
        assert!(matches!(
            lazy(|cx| io.poll_flush(cx, false)).await,
            Poll::Ready(Ok(()))
        ));
        // full flush is disabled
        assert!(!io.st().flags.is_write_flush());
        // back-pressure is disabled
        assert!(!io.st().flags.is_wr_backpressure());

        // == Terminate
        io.terminate();
        // read task is woken
        assert!(!io.st().write_task.is_set());
        // write task ready
        assert_eq!(
            lazy(|cx| ctx.poll_write_ready(cx)).await,
            Poll::Ready(Readiness::Terminate)
        );
        // flush returns error
        let Poll::Ready(Err(err)) = lazy(|cx| io.poll_flush(cx, false)).await else {
            panic!()
        };
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        // statis returns error
        assert!(matches!(
            lazy(|cx| io.poll_status_update(cx)).await,
            Poll::Ready(IoStatusUpdate::PeerGone(None))
        ));
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
        assert!(Iops::is_registered(&io));
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

    #[ntex::test]
    async fn shutdown() {
        // layer drops all unprocessed data after filter shutdown
        #[derive(Debug)]
        struct F;

        impl FilterLayer for F {
            fn process_read_buf(&self, _: &mut FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }
            fn process_write_buf(&self, _: &mut FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }
        }

        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_buf(8, 4, 16)),
        );
        let st = io.st();
        assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
        assert!(st.dispatch_task.is_set());
        assert!(!st.flags.is_closed());
        assert!(!st.flags.is_stopping_filters());

        let ctx = IoContext::new(io.get_ref());

        // == init shutdown
        io.close();
        assert!(!st.flags.is_closed());
        assert!(st.flags.is_stopping_filters());
        // encoding is not allowed in shutting down stage
        let err = io.with_write_buf(|_| 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);

        st.buffer.add_layer();
        let layer = Layer::new(F, Base::new(io.get_ref()));

        st.buffer.with_write_src(|p| p.put_slice(b"123"));
        assert_eq!(st.buffer.write_buf_size(), 3);
        let res = st.buffer.with_filter(io.as_ref(), |f| layer.shutdown(f));
        assert!(matches!(res, Ok(Poll::Ready(()))));
        assert_eq!(st.buffer.write_buf_size(), 0);

        // == terminate
        ctx.stop(None);
        assert!(st.flags.is_closed());
        assert!(st.flags.is_terminated());
        assert!(st.flags.is_stopping_filters());

        let err = io.with_write_buf(|_| 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    }
}
