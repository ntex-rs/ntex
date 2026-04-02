use std::cell::{Cell, UnsafeCell};
use std::future::{Future, poll_fn};
use std::task::{Context, Poll};
use std::{fmt, hash, io, marker, mem, ops, pin::Pin, ptr, rc::Rc};

use ntex_codec::{Decoder, Encoder};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_util::{future::Either, task::LocalWaker};

use crate::buf::Stack;
use crate::cfg::{BufConfig, IoConfig};
use crate::filter::{Base, Filter, Layer};
use crate::filterptr::FilterPtr;
use crate::flags::Flags;
use crate::seal::{IoBoxed, Sealed};
use crate::timer::TimerHandle;
use crate::{Decoded, FilterLayer, Handle, IoContext, IoStatusUpdate, IoStream, RecvError};

/// Interface object to underlying io stream
pub struct Io<F = Base>(UnsafeCell<IoRef>, marker::PhantomData<F>);

#[derive(Clone)]
pub struct IoRef(pub(super) Rc<IoState>);

pub(crate) struct IoState {
    filter: FilterPtr,
    pub(super) cfg: Cfg<IoConfig>,
    pub(super) flags: Cell<Flags>,
    pub(super) error: Cell<Option<io::Error>>,
    pub(super) read_task: LocalWaker,
    pub(super) write_task: LocalWaker,
    pub(super) dispatch_task: LocalWaker,
    pub(super) buffer: Stack,
    pub(super) handle: Cell<Option<Box<dyn Handle>>>,
    pub(super) timeout: Cell<TimerHandle>,
    #[allow(clippy::box_collection)]
    pub(super) on_disconnect: Cell<Option<Box<Vec<LocalWaker>>>>,
}

impl IoState {
    pub(super) fn filter(&self) -> &dyn Filter {
        self.filter.get()
    }

    pub(super) fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    pub(super) fn remove_flags(&self, f: Flags) -> bool {
        let mut flags = self.flags.get();
        if flags.intersects(f) {
            flags.remove(f);
            self.flags.set(flags);
            true
        } else {
            false
        }
    }

    pub(super) fn notify_timeout(&self) {
        let mut flags = self.flags.get();
        if !flags.contains(Flags::DSP_TIMEOUT) {
            flags.insert(Flags::DSP_TIMEOUT);
            self.flags.set(flags);
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

    pub(super) fn io_stopped(&self, err: Option<io::Error>) {
        if !self.flags.get().is_stopped() {
            log::trace!(
                "{}: {:?} Io error {:?} flags: {:?}",
                self.cfg.tag(),
                ptr::from_ref(self),
                err,
                self.flags.get()
            );

            if err.is_some() {
                self.error.set(err);
            }
            self.read_task.wake();
            self.write_task.wake();
            self.notify_disconnect();
            self.handle.take();
            self.insert_flags(
                Flags::IO_STOPPED
                    | Flags::IO_STOPPING
                    | Flags::IO_STOPPING_FILTERS
                    | Flags::BUF_R_READY,
            );
            if !self.dispatch_task.wake_checked() {
                log::trace!(
                    "{}: {:?} Dispatcher is not registered, flags: {:?}",
                    self.cfg.tag(),
                    ptr::from_ref(self),
                    self.flags.get()
                );
            }
        }
    }

    /// Gracefully shutdown read and write io tasks
    pub(super) fn init_shutdown(&self) {
        if !self
            .flags
            .get()
            .intersects(Flags::IO_STOPPED | Flags::IO_STOPPING | Flags::IO_STOPPING_FILTERS)
        {
            log::trace!(
                "{}: Initiate io shutdown {:?}",
                self.cfg.tag(),
                self.flags.get()
            );
            self.insert_flags(Flags::IO_STOPPING_FILTERS);
            self.read_task.wake();
        }
    }

    #[inline]
    pub(super) fn read_buf(&self) -> &BufConfig {
        self.cfg.read_buf()
    }

    #[inline]
    pub(super) fn write_buf(&self) -> &BufConfig {
        self.cfg.write_buf()
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
        let inner = Rc::new(IoState {
            cfg: cfg.into().get::<IoConfig>(),
            filter: FilterPtr::null(),
            flags: Cell::new(Flags::WR_PAUSED),
            error: Cell::new(None),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            buffer: Stack::new(),
            handle: Cell::new(None),
            timeout: Cell::new(TimerHandle::default()),
            on_disconnect: Cell::new(None),
        });
        inner.filter.set(Base::new(IoRef(inner.clone())));

        let io_ref = IoRef(inner);

        // start io tasks
        let hnd = io.start(IoContext::new(&io_ref));
        io_ref.0.handle.set(hnd);

        Io(UnsafeCell::new(io_ref), marker::PhantomData)
    }
}

impl<I: IoStream> From<I> for Io {
    #[inline]
    fn from(io: I) -> Io {
        Io::new(io, SharedCfg::default())
    }
}

impl<F> Io<F> {
    #[inline]
    #[must_use]
    /// Clone current io object.
    ///
    /// Current io object becomes closed.
    pub fn take(&self) -> Self {
        Self(UnsafeCell::new(self.take_io_ref()), marker::PhantomData)
    }

    fn take_io_ref(&self) -> IoRef {
        let inner = Rc::new(IoState {
            cfg: SharedCfg::default().get::<IoConfig>(),
            filter: FilterPtr::null(),
            flags: Cell::new(
                Flags::IO_STOPPED | Flags::IO_STOPPING | Flags::IO_STOPPING_FILTERS,
            ),
            error: Cell::new(None),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            buffer: Stack::new(),
            handle: Cell::new(None),
            timeout: Cell::new(TimerHandle::default()),
            on_disconnect: Cell::new(None),
        });
        unsafe { mem::replace(&mut *self.0.get(), IoRef(inner)) }
    }

    #[inline]
    #[doc(hidden)]
    /// Get current state flags
    pub fn flags(&self) -> Flags {
        self.st().flags.get()
    }

    #[inline]
    /// Get instance of `IoRef`
    pub fn get_ref(&self) -> IoRef {
        self.io_ref().clone()
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
            self.st().cfg.replace(cfg.into().get::<IoConfig>());
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
        let state = self.take_io_ref();
        state.0.filter.map_filter::<F, U, R>(f);

        Io(UnsafeCell::new(state), marker::PhantomData)
    }
}

impl<F> Io<F> {
    #[inline]
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
        if !st.flags.get().contains(Flags::RD_PAUSED) {
            st.read_task.wake();
            st.insert_flags(Flags::RD_PAUSED);
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
        let mut flags = st.flags.get();

        if flags.is_stopped() {
            Poll::Ready(Err(st.error_or_disconnected()))
        } else {
            st.dispatch_task.register(cx.waker());

            let ready = flags.is_read_buf_ready();
            if flags.cannot_read() {
                flags.cleanup_read_flags();
                st.read_task.wake();
                st.flags.set(flags);
                if ready {
                    Poll::Ready(Ok(Some(())))
                } else {
                    Poll::Pending
                }
            } else if ready {
                flags.remove(Flags::BUF_R_READY);
                st.flags.set(flags);
                Poll::Ready(Ok(Some(())))
            } else {
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
            if st.remove_flags(Flags::RD_NOTIFY) {
                Poll::Ready(Ok(Some(())))
            } else {
                st.insert_flags(Flags::RD_NOTIFY);
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
        let decoded = self
            .decode_item(codec)
            .map_err(|err| RecvError::Decoder(err))?;

        if decoded.item.is_some() {
            Ok(decoded)
        } else {
            let st = self.st();
            let flags = st.flags.get();
            if flags.is_stopped() {
                Err(RecvError::PeerGone(st.error()))
            } else if flags.contains(Flags::DSP_TIMEOUT) {
                st.remove_flags(Flags::DSP_TIMEOUT);
                Err(RecvError::KeepAlive)
            } else if flags.contains(Flags::BUF_W_BACKPRESSURE) {
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
        let flags = self.flags();

        let len = st.buffer.write_destination_size();
        if len > 0 {
            if full {
                st.insert_flags(Flags::BUF_W_MUST_FLUSH);
                st.dispatch_task.register(cx.waker());
                return if flags.is_stopped() {
                    Poll::Ready(Err(st.error_or_disconnected()))
                } else {
                    Poll::Pending
                };
            } else if len >= st.write_buf().half {
                st.insert_flags(Flags::BUF_W_BACKPRESSURE);
                st.dispatch_task.register(cx.waker());
                return if flags.is_stopped() {
                    Poll::Ready(Err(st.error_or_disconnected()))
                } else {
                    Poll::Pending
                };
            }
        }
        if flags.is_stopped() {
            Poll::Ready(Err(st.error_or_disconnected()))
        } else {
            st.remove_flags(Flags::BUF_W_MUST_FLUSH | Flags::BUF_W_BACKPRESSURE);
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    /// Gracefully shutdown io stream
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let st = self.st();
        let flags = st.flags.get();

        if flags.is_stopped() {
            if let Some(err) = st.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(()))
            }
        } else {
            if !flags.contains(Flags::IO_STOPPING_FILTERS) {
                st.init_shutdown();
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
        let result = self.poll_status_update(cx);
        if !result.is_pending() {
            self.st().dispatch_task.register(cx.waker());
        }
        result
    }

    #[inline]
    /// Wait for status updates
    pub fn poll_status_update(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        let st = self.st();
        let flags = st.flags.get();
        if flags.intersects(Flags::IO_STOPPED | Flags::IO_STOPPING) {
            Poll::Ready(IoStatusUpdate::PeerGone(st.error()))
        } else if flags.contains(Flags::DSP_TIMEOUT) {
            st.remove_flags(Flags::DSP_TIMEOUT);
            Poll::Ready(IoStatusUpdate::KeepAlive)
        } else if flags.contains(Flags::BUF_W_BACKPRESSURE) {
            Poll::Ready(IoStatusUpdate::WriteBackpressure)
        } else {
            st.dispatch_task.register(cx.waker());
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
        let st = self.st();
        self.stop_timer();

        if st.filter.is_set() {
            // filter is unsafe and must be dropped explicitly,
            // and won't be dropped without special attention
            if !st.flags.get().is_stopped() {
                log::trace!(
                    "{}: Io is dropped, force stopping io streams {:?}",
                    st.cfg.tag(),
                    st.flags.get()
                );
            }

            self.force_close();
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
        Self::new_inner(inner.flags.get().is_stopped(), inner)
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
        if self.token == usize::MAX || self.inner.flags.get().is_stopped() {
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
    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;

    use super::*;
    use crate::testing::IoTest;

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[ntex::test]
    async fn test_basics() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::from(server);
        assert!(server.eq(&server));
        assert!(server.io_ref().eq(server.io_ref()));

        assert!(format!("{:?}", Flags::IO_STOPPED).contains("IO_STOPPED"));
        assert!(Flags::IO_STOPPED == Flags::IO_STOPPED);
        assert!(Flags::IO_STOPPED != Flags::IO_STOPPING);
    }

    #[ntex::test]
    async fn test_recv() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::from(server);

        server.st().notify_timeout();
        let err = server.recv(&BytesCodec).await.err().unwrap();
        assert!(format!("{err:?}").contains("Timeout"));

        client.write(TEXT);
        server.st().insert_flags(Flags::BUF_W_BACKPRESSURE);
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
}
