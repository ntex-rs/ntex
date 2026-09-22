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
use crate::utils::Extensions;
use crate::{Decoded, FilterLayer, Handle, IoStatusUpdate, IoStream, RecvError};

/// Buffered, filterable interface to an underlying I/O stream.
///
/// `Io` is the main handle for a connection. The runtime fills its read buffer
/// and drains its write buffer, while protocol code uses this handle to decode
/// requests and encode responses.
///
/// Reads and writes go through buffers rather than directly to the socket.
/// When the read buffer grows too large, ntex pauses the transport.
/// [`read_more`](Self::read_more) can resume it, so calling that method asks
/// for more input; it does more than check whether data is already available.
///
/// If the peer cleanly closes its read side, any buffered input remains
/// available and responses can still be written. [`shutdown`](Self::shutdown)
/// closes the connection gracefully and waits for the runtime to finish.
/// [`terminate`](IoRef::terminate) closes it immediately.
///
/// The `F` parameter keeps track of the installed filters. Adding or mapping a
/// filter changes the type. Use [`seal`](Self::seal) or [`boxed`](Self::boxed)
/// when the concrete filter type does not need to be exposed.
///
/// Use [`get_ref`](Self::get_ref) to share access to the connection. The
/// returned [`IoRef`] points to the same buffers and state; it does not create
/// another connection. Dropping `Io` terminates the connection even if an
/// `IoRef` is still alive.
pub struct Io<F = Base>(UnsafeCell<IoRef>, marker::PhantomData<F>);

/// A cheap, cloneable handle to an [`Io`] connection.
///
/// All clones point to the same connection. Changes to its buffers,
/// configuration, timers, errors, or shutdown state are visible through every
/// clone. Cloning `IoRef` never clones the socket.
///
/// Use it to inspect the connection, access its buffers, queue output, or start
/// graceful or immediate shutdown. These methods are also available directly
/// on `Io`, which dereferences to `IoRef`.
///
/// Keeping an `IoRef` alive does not keep the connection open after its `Io`
/// owner is dropped. Like `Io`, it stays on the local runtime thread and is
/// neither `Send` nor `Sync`.
#[derive(Clone)]
pub struct IoRef(pub(super) Rc<IoState>);

/// Saturating conversion used for the in-flight write counter.
fn as_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

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
    /// Bytes handed to the transport that have not reached the peer yet.
    ///
    /// Completion based transports take ownership of write pages and keep them
    /// until the operation completes, so those bytes are no longer in
    /// `buffer`. They are still outstanding output and must be accounted for.
    pub(super) wr_inflight: Cell<u32>,
    pub(super) extensions: Extensions,
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
        self.extensions.notify_disconnect();
    }

    /// Get the current I/O error.
    pub(super) fn error(&self) -> Option<io::Error> {
        if let Some(err) = self.error.take() {
            let cloned = if let Some(code) = err.raw_os_error() {
                io::Error::from_raw_os_error(code)
            } else {
                io::Error::new(err.kind(), format!("{err}"))
            };
            self.error.set(Some(cloned));
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
        // the shutdown deadline carries over into the transport shutdown
        // phase, so that a single `shutdown_timeout` bounds both phases
        self.wake_read_task();
        self.wake_write_task();
        self.wake_dispatch_task();
        self.flags.set_filters_stopped();
    }

    fn set_error(&self, err: Option<io::Error>) {
        if let Some(err) = err {
            if let Some(current) = self.error.take() {
                self.error.set(Some(current));
            } else {
                self.error.set(Some(err));
            }
        }
    }

    pub(super) fn set_shutdown_error(&self, err: io::Error) {
        self.set_error(Some(err));
    }

    pub(super) fn terminate_connection(&self, err: Option<io::Error>) {
        self.set_error(err);
        if !self.flags.is_terminated() && !self.flags.is_terminating() {
            log::trace!("{}: Terminate io", self.cfg.tag());
            self.flags.set_terminate();
            // buffers held by the transport are gone with it
            self.wr_inflight.set(0);
            self.wake_read_task();
            self.wake_write_task();
            self.wake_dispatch_task();
            self.handle.take();
        }
    }

    pub(super) fn stop_connection(&self, err: Option<io::Error>) {
        if !self.flags.is_terminated() {
            log::trace!("{}: Stop io with error {:?}", self.cfg.tag(), err);
            self.set_error(err);
            self.flags.set_stopped();
            // buffers held by the transport are gone with it
            self.wr_inflight.set(0);
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

    pub(super) fn should_disable_rd_backpressure(&self, size: usize) -> bool {
        size <= self.cfg.read_buf().half
    }

    pub(super) fn is_wr_backpressure_needed(&self, size: usize) -> bool {
        size >= self.cfg.write_buf().high
    }

    pub(super) fn should_disable_wr_backpressure(&self, size: usize) -> bool {
        size <= self.cfg.write_buf().half
    }

    /// Total output that has not reached the peer yet.
    ///
    /// This is the buffered output plus whatever the transport has taken
    /// ownership of but not written out. Flush completion, write
    /// back-pressure and the shutdown drain are all decided on this value,
    /// because until it reaches zero the peer has not seen everything.
    pub(super) fn write_outstanding(&self) -> usize {
        self.buffer.write_buf_size() + self.wr_inflight.get() as usize
    }

    /// Records bytes taken by, or returned from, the transport.
    pub(super) fn track_wr_inflight(&self, before: usize, after: usize) {
        let inflight = self.wr_inflight.get();
        if after < before {
            self.wr_inflight
                .set(inflight.saturating_add(as_u32(before - after)));
        } else {
            // the transport returned unwritten output to the buffer
            self.wr_inflight
                .set(inflight.saturating_sub(as_u32(after - before)));
        }
    }

    /// Records output that reached the peer.
    pub(super) fn wr_inflight_written(&self, written: usize) {
        self.wr_inflight
            .set(self.wr_inflight.get().saturating_sub(as_u32(written)));
    }

    pub(super) fn wake_read_task(&self) {
        self.read_task.wake();
    }

    pub(super) fn wake_write_task(&self) {
        #[cfg(feature = "trace")]
        log::trace!("{}: Wake write task, flags:{:?}", self.tag(), self.flags);
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
            wr_inflight: Cell::new(0),
            extensions: Extensions::default(),
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
            wr_inflight: Cell::new(0),
            extensions: Extensions::default(),
        }))
    }
}

impl<F> Io<F> {
    #[inline]
    /// Returns a cloneable reference to this connection's shared state.
    pub fn get_ref(&self) -> IoRef {
        self.io_ref().clone()
    }

    #[inline]
    #[must_use]
    /// Transfers the live connection state into a new `Io` object.
    ///
    /// This does not clone the connection. `self` is replaced with a stopped
    /// placeholder and should no longer be used for I/O.
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
    /// Replaces this connection's shared I/O configuration.
    ///
    /// The write-buffer page size and eager-write enablement are updated
    /// immediately. Existing allocated buffers and an already registered timer
    /// are not recreated.
    ///
    /// # Safety
    ///
    /// No reference obtained from [`IoRef::cfg`] for this connection may be
    /// live when this method is called or used afterward. Replacing the
    /// configuration may release the allocation backing those references.
    pub unsafe fn set_config<T: Into<SharedCfg>>(&self, cfg: T) {
        unsafe {
            let cfg = cfg.into().get::<IoConfig>();
            self.st().buffer.set_page_size(cfg.write_page_size());
            self.st()
                .flags
                .set_direct_wr_enabled(cfg.write_buf_threshold() > 0);
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
    /// Converts the current I/O stream into a sealed version.
    pub fn seal(self) -> Io<Sealed> {
        let state = self.take_io_ref();
        state.0.filter.seal::<F>();

        Io(UnsafeCell::new(state), marker::PhantomData)
    }

    #[inline]
    /// Converts the current I/O stream into a boxed version.
    pub fn boxed(self) -> IoBoxed {
        self.seal().into()
    }

    #[inline]
    /// Adds a new processing layer to the current filter chain.
    pub fn add_filter<U>(self, nf: U) -> Io<Layer<U, F>>
    where
        U: FilterLayer,
    {
        self.with_callbacks(|cb| cb.before_processing(&self));

        // Write buffer processing may be delayed,
        // call the filter chain to process pending writes
        if let Err(e) = self.st().buffer.process_write_buf_no_cb(&self) {
            self.st().terminate_connection(Some(e));
        }

        let state = self.take_io_ref();

        // Add the buffers layer.
        //
        // Safety: no references into the buffer storage are retained.
        // All APIs first remove the buffer from storage before processing it.
        unsafe { &mut *(Rc::as_ptr(&state.0).cast_mut()) }
            .buffer
            .add_layer(state.0.cfg.write_page_size());

        // Replace current filter
        state.0.filter.add_filter::<F, U>(nf);

        let io = Io(UnsafeCell::new(state), marker::PhantomData);

        // push read data into new filter
        if let Err(e) = io.st().buffer.process_read_buf_no_cb(&io) {
            io.st().terminate_connection(Some(e));
        }
        io.with_callbacks(|cb| cb.after_processing(&io));

        io
    }

    /// Wraps the current layer with a wrapper.
    pub fn map_filter<U, R>(self, f: U) -> Io<R>
    where
        U: FnOnce(F) -> R,
        R: Filter,
    {
        self.with_callbacks(|cb| cb.before_processing(&self));

        // Write buffer processing may be delayed,
        // call the filter chain to process pending writes
        if let Err(e) = self.st().buffer.process_write_buf(&self) {
            self.st().terminate_connection(Some(e));
        }

        let state = self.take_io_ref();
        state.0.filter.map_filter::<F, U, R>(f);

        let io = Io(UnsafeCell::new(state), marker::PhantomData);
        io.with_callbacks(|cb| cb.after_processing(&io));
        io
    }
}

impl<F> Io<F> {
    /// Reads and decodes the next item from the incoming stream.
    ///
    /// Returns `Ok(None)` when the connection closed before another item could
    /// be decoded and nothing was left undecoded, whether the peer
    /// disconnected or the shutdown was started locally.
    ///
    /// If the peer closed its write half while the codec still held a partial
    /// item, the stream was truncated and this returns
    /// [`io::ErrorKind::UnexpectedEof`] in [`Either::Right`] rather than
    /// `Ok(None)`, so that a cut-off frame is not mistaken for a clean end of
    /// stream. Undecodable bytes left after a locally started shutdown are not
    /// treated as truncation.
    ///
    /// Codec errors are returned in [`Either::Left`]; transport errors and
    /// dispatcher timeouts are returned in [`Either::Right`].
    ///
    /// If write backpressure prevents further reads, this method first waits
    /// for the write buffer to fall below its configured threshold.
    pub async fn recv<U>(&self, codec: &U) -> Result<Option<U::Item>, Either<U::Error, io::Error>>
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
                Err(RecvError::PeerGone(None)) => {
                    let st = self.st();
                    if st.flags.is_read_eof() && st.buffer.read_dst_size() != 0 {
                        Err(Either::Right(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "bytes remaining on stream",
                        )))
                    } else {
                        Ok(None)
                    }
                }
            };
        }
    }

    /// Reads exactly enough bytes from this I/O stream to fill `dst`.
    ///
    /// If there is not enough data available, waits for incoming data.
    /// If clean EOF or an error-free shutdown occurs before `dst` is filled,
    /// this returns [`io::ErrorKind::UnexpectedEof`]. Transport errors are
    /// passed through unchanged.
    ///
    /// Each wait goes through [`read_more`](Self::read_more), so this releases
    /// read backpressure unconditionally rather than waiting for the read
    /// buffer to drain to half the high watermark.
    pub async fn read_exact(&self, dst: &mut [u8]) -> io::Result<()> {
        loop {
            let completed = self.with_read_dst(|buf| {
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
            // No more bytes will arrive after clean EOF or shutdown.
            if self.read_more().await?.is_none() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Disconnected"));
            }
        }
    }

    #[inline]
    /// Waits until application-facing data is available and allows the
    /// transport to read more data.
    ///
    /// If reads are paused or under backpressure, calling this method resumes
    /// the read task. This is not a passive check of the current buffer, and
    /// read backpressure is released however much data is still buffered.
    ///
    /// Returns `Ok(Some(()))` when input that has not been reported yet is
    /// available, `Ok(None)` when no further input will be reported, and `Err`
    /// if the transport failed. See [`poll_read_more`](Self::poll_read_more)
    /// for what `None` means after a clean EOF.
    pub async fn read_more(&self) -> io::Result<Option<()>> {
        poll_fn(|cx| self.poll_read_more(cx)).await
    }

    #[inline]
    /// Waits for the next read from the transport.
    ///
    /// Use this when a filter needs more source bytes. Unlike
    /// [`read_more`](Self::read_more), data already waiting in the application
    /// buffer does not complete this wait. If the read task is paused, this
    /// method wakes it.
    ///
    /// Returns `Some(())` when the transport provides more input. If clean EOF
    /// leaves final data in the application buffer, it returns `Some(())` once
    /// and `None` afterward. A transport error is returned unchanged.
    pub async fn read_notify(&self) -> io::Result<Option<()>> {
        poll_fn(|cx| self.poll_read_notify(cx)).await
    }

    #[inline]
    /// Encodes an item and sends it to the peer, fully flushing the write buffer.
    pub async fn send<U>(&self, item: U::Item, codec: &U) -> Result<(), Either<U::Error, io::Error>>
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
    /// Wakes the write task and requests a flush of queued output.
    ///
    /// This is the asynchronous counterpart to `poll_flush`. A full flush
    /// completes once all output has reached the peer, including output a
    /// transport has taken ownership of but not yet written.
    pub async fn flush(&self, full: bool) -> io::Result<()> {
        poll_fn(|cx| self.poll_flush(cx, full)).await
    }

    #[inline]
    /// Gracefully shuts down the I/O stream.
    ///
    /// Shutdown runs in two phases, bounded together by a single
    /// [`IoConfig::set_shutdown_timeout`]. First the filters shut down while
    /// both directions stay open, so a filter can emit its closing data and
    /// read the peer's. Then the transport drains the remaining output, pauses
    /// the read side, and closes the connection.
    ///
    /// This completes once the transport backend has finished its shutdown
    /// operation, not merely once the output has been drained.
    ///
    /// [`IoConfig::set_shutdown_timeout`]: crate::IoConfig::set_shutdown_timeout
    pub async fn shutdown(&self) -> io::Result<()> {
        poll_fn(|cx| self.poll_shutdown(cx)).await
    }

    #[inline]
    /// Polls for application-facing data and allows the transport to read more.
    ///
    /// If reads are paused or under backpressure, this resumes the read task.
    /// It therefore changes the read state and should not be used as a passive
    /// buffer check.
    ///
    /// The release is unconditional: unlike consumption through
    /// [`IoRef::decode`], [`IoRef::with_buf`], [`IoRef::with_read_src`] or
    /// [`IoRef::with_read_dst`], which waits for the read buffer to fall to at
    /// most half the high watermark, this releases read backpressure however
    /// much data is still buffered. Asking for more input
    /// is taken as the dispatcher declaring itself able to accept it.
    ///
    /// # Returns
    ///
    /// - `Poll::Pending` while waiting for more data.
    /// - `Poll::Ready(Ok(Some(())))` when input that has not been reported yet
    ///   is available.
    /// - `Poll::Ready(Ok(None))` when no further input will be reported: after
    ///   a clean EOF once the available input has been reported, or when the
    ///   stream closes without an error. Clean EOF leaves the write half open.
    ///
    ///   This reports arrivals, not buffer contents. Input that has already
    ///   been reported stays in the read buffer and remains decodable through
    ///   [`IoRef::decode`] and [`IoRef::with_read_dst`], so `None` does not
    ///   imply that the read buffer is empty.
    /// - `Poll::Ready(Err(e))` if the transport failed.
    pub fn poll_read_more(&self, cx: &mut Context<'_>) -> Poll<io::Result<Option<()>>> {
        let st = self.st();

        if st.flags.is_closed() {
            if let Some(err) = st.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(None))
            }
        } else {
            let ready = st.flags.is_read_ready();

            if st.flags.is_read_eof() && !ready {
                return Poll::Ready(Ok(None));
            }

            // If the dispatcher requests more data but no read occurs,
            // restart the read task.
            if st.flags.is_read_paused_or_backpressure() {
                st.flags.unset_read_ready_and_backpressure();
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
                st.dispatch_task.register(cx.waker());
                Poll::Pending
            }
        }
    }

    #[inline]
    /// Polls for the next read from the transport.
    ///
    /// This is the polling version of [`read_notify`](Self::read_notify).
    /// Existing application data does not make it ready. When another transport
    /// read is needed, this wakes the read task and registers the current waker.
    ///
    /// `Some(())` means that more input arrived. Clean EOF may produce one last
    /// `Some(())` when filters leave final application data; later polls return
    /// `None`. Transport errors are returned unchanged.
    ///
    /// Paused or back-pressured reads are resumed through
    /// [`poll_read_more`](Self::poll_read_more), which releases read
    /// backpressure unconditionally.
    pub fn poll_read_notify(&self, cx: &mut Context<'_>) -> Poll<io::Result<Option<()>>> {
        let st = self.st();
        if st.flags.is_stopping_or_terminating() {
            if let Some(err) = st.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(None))
            }
        } else if st.flags.is_read_eof() {
            let notified = st.flags.check_read_notifed();
            if notified && st.flags.is_read_ready() {
                Poll::Ready(Ok(Some(())))
            } else {
                Poll::Ready(Ok(None))
            }
        } else if st.flags.check_read_notifed() {
            Poll::Ready(Ok(Some(())))
        } else {
            st.flags.set_read_notify();
            // Resumes the read task if reads are paused. The result is
            // discarded on purpose: buffered application data does not
            // complete this wait, and the eof and closed cases are handled
            // by the branches above.
            let _ = self.poll_read_more(cx);
            st.dispatch_task.register(cx.waker());
            Poll::Pending
        }
    }

    #[inline]
    /// Decodes the next item from the incoming byte stream.
    ///
    /// Returns `Poll::Pending` when the codec needs more input, after going
    /// through [`poll_read_more`](Self::poll_read_more), which wakes the read
    /// task and releases read backpressure unconditionally.
    ///
    /// An error return does not register the waker.
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
    /// Attempts to decode an item and reports buffer progress.
    ///
    /// `Decoded::consumed` is the number of bytes consumed by this decode
    /// attempt and `Decoded::remains` is the number left in the
    /// application-facing read buffer. If the codec needs more input, this
    /// returns `Ok` with `item` set to `None` after arranging for `cx` to be
    /// woken when progress is possible.
    ///
    /// An error return does not register the waker. A successfully decoded item
    /// takes precedence over timeout, backpressure, and peer-disconnect status
    /// observed during the same poll.
    ///
    /// When the codec needs more input this goes through
    /// [`poll_read_more`](Self::poll_read_more), which releases read
    /// backpressure unconditionally.
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
        } else if st.flags.is_stopping() || st.flags.is_terminating() {
            Err(RecvError::PeerGone(st.error()))
        } else if st.flags.check_dispatcher_timeout() {
            Err(RecvError::KeepAlive)
        } else if st.flags.is_wr_backpressure() {
            Err(RecvError::WriteBackpressure)
        } else {
            match self.poll_read_more(cx) {
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
    /// A full flush waits until all output has reached the peer.
    ///
    /// Otherwise this returns immediately while the outstanding size is below
    /// the configured high watermark. Reaching that watermark enables write
    /// backpressure, and the call then waits until the outstanding size falls
    /// to half of it.
    ///
    /// Output that a completion based transport has taken ownership of counts
    /// as outstanding until it reaches the peer, so a full flush does not
    /// complete while a write is still in flight.
    pub fn poll_flush(&self, cx: &mut Context<'_>, full: bool) -> Poll<io::Result<()>> {
        let st = self.st();

        // flush filter state
        st.buffer.process_write_buf_force(self)?;
        self.consolidate_write_state(false)?;

        let len = st.write_outstanding();
        if len > 0 {
            if st.flags.is_closed() {
                return Poll::Ready(Err(st.error_or_disconnected()));
            } else if full {
                st.flags.set_wants_write_flush();
                st.dispatch_task.register(cx.waker());
                return Poll::Pending;
            } else if st.flags.is_wr_backpressure() {
                if !st.should_disable_wr_backpressure(len) {
                    st.dispatch_task.register(cx.waker());
                    return Poll::Pending;
                }
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
    /// Polls graceful shutdown through transport completion.
    ///
    /// `Poll::Ready` is returned only after the transport backend marks the
    /// connection stopped, not merely when filter shutdown and flushing finish.
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let st = self.st();

        if st.flags.is_terminated() {
            if let Some(err) = st.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(()))
            }
        } else {
            if !st.flags.is_terminating() && !st.flags.is_stopping_filters() {
                st.start_shutdown();
            }
            // Reads must keep running during shutdown so that the transport can
            // observe a peer EOF and filters can complete their shutdown
            // handshake. `BUF_R_READY` is deliberately left alone: it marks
            // input the dispatcher has not consumed yet.
            //
            // Only the pause is cleared. Backpressure means the read buffer is
            // full, which the shutdown must still respect: clearing it on every
            // poll would let a peer that keeps sending grow the buffer without
            // bound, and would hide the blocked shutdown detection in
            // `poll_filters_shutdown`, which tests for exactly that flag.
            st.flags.unset_read_paused();

            st.wake_read_task();
            st.wake_write_task();
            st.dispatch_task.register(cx.waker());
            Poll::Pending
        }
    }

    #[inline]
    /// Pauses the read task and polls for a status update.
    ///
    /// The transport stops reading until the pause is cancelled. There is no
    /// explicit resume: the pause is cancelled implicitly by any operation that
    /// touches the read buffer or asks for more input, namely
    /// [`read_more`](Self::read_more), [`poll_read_more`](Self::poll_read_more),
    /// [`IoRef::decode`], [`IoRef::decode_item`], and [`IoRef::with_read_dst`].
    /// Releasing read backpressure cancels it as well. Because those methods
    /// are available through every [`IoRef`] clone, the pause holds only while
    /// no other holder touches the read buffer.
    ///
    /// See [`poll_status_update`](Self::poll_status_update) for the reported
    /// updates.
    pub fn poll_read_pause(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        let st = self.st();
        if !st.flags.is_read_paused() {
            st.wake_read_task();
            st.flags.set_read_paused();
        }
        self.poll_status_update(cx)
    }

    #[inline]
    /// Polls for available status updates.
    ///
    /// `KeepAlive` consumes the pending dispatcher-timeout notification.
    /// `WriteBackpressure` is reported while backpressure is active. The poll
    /// that observes the write buffer falling below its release threshold
    /// releases backpressure and reports no status update, matching
    /// [`poll_flush`](Self::poll_flush). `PeerGone` is returned once the
    /// connection has closed, whether the peer disconnected, the transport
    /// failed, or the shutdown was started locally.
    pub fn poll_status_update(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        let st = self.st();
        st.dispatch_task.register(cx.waker());
        if st.flags.is_closed() {
            Poll::Ready(IoStatusUpdate::PeerGone(st.error()))
        } else if st.flags.check_dispatcher_timeout() {
            Poll::Ready(IoStatusUpdate::KeepAlive)
        } else if st.flags.is_wr_backpressure() {
            // write backpressure is enabled and outstanding output is smaller than half
            if st.should_disable_wr_backpressure(st.write_outstanding()) {
                st.flags.unset_wr_backpressure();
                Poll::Pending
            } else {
                Poll::Ready(IoStatusUpdate::WriteBackpressure)
            }
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
/// A future that resolves when the complete I/O stream disconnects.
///
/// A clean peer read EOF does not resolve this future while the write half
/// remains usable. It resolves only once the transport backend reports that
/// teardown has finished; requesting shutdown or termination is not by itself
/// enough.
#[must_use = "OnDisconnect do nothing unless polled"]
pub struct OnDisconnect {
    token: usize,
    inner: Rc<IoState>,
}

impl OnDisconnect {
    pub(super) fn new(inner: Rc<IoState>) -> Self {
        Self::new_inner(inner.flags.is_terminated(), inner)
    }

    fn new_inner(disconnected: bool, inner: Rc<IoState>) -> Self {
        let token = if disconnected {
            usize::MAX
        } else {
            inner.extensions.register_disconnect()
        };
        Self { token, inner }
    }

    #[inline]
    /// Checks if the I/O stream is disconnected.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.token == usize::MAX || self.inner.flags.is_terminated() {
            Poll::Ready(())
        } else {
            self.inner
                .extensions
                .poll_disconnect(self.token, cx.waker())
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
    use std::{cell::Cell, rc::Rc};

    use ntex_bytes::{BufMut, BytePages, Bytes, BytesMut};
    use ntex_codec::BytesCodec;
    use ntex_util::{future::lazy, time::Millis, time::sleep, time::timeout};

    use super::*;
    use crate::{FilterBuf, IoContext, IoTaskStatus, Readiness, ops::Iops, testing::IoTest};

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
    async fn test_stop_timer_clears_timeout_notification() {
        let (_client, server) = IoTest::create();
        let server = Io::new(server, SharedCfg::new("SRV"));

        server.start_timer(ntex_util::time::Seconds(10));
        server.notify_timeout();
        server.stop_timer();

        assert!(lazy(|cx| server.poll_status_update(cx)).await.is_pending());
    }

    #[ntex::test]
    async fn test_read() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::new(server, SharedCfg::new("SRV"));

        client.write(b"1234");
        let mut buf: [u8; 4] = [0, 0, 0, 0];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"1234");

        // disconnect during read
        let fut = ntex_rt::spawn(async move {
            let mut buf: [u8; 4] = [0, 0, 0, 0];
            let err = server.read_exact(&mut buf).await.unwrap_err();
            (server, err)
        });
        client.close().await;
        let (server, err) = fut.await.unwrap();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        let err = server.read_exact(&mut [0]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[ntex::test]
    async fn test_read_partial_eof() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::new(server, SharedCfg::new("SRV"));

        client.write(b"12");
        client.close().await;

        let err = server.read_exact(&mut [0; 4]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        let mut buf = [0; 2];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"12");

        let err = server.read_exact(&mut [0]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
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
        assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());
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
        assert!(!io.is_rd_backpressure());
        assert!(!io.is_wr_backpressure());

        // == Enable backpressure
        ctx.release_read_buf(
            BytesMut::copy_from_slice(b"1234567890"),
            Poll::Ready(Ok(10)),
        );

        // dispatcher is woken
        assert!(!io.st().dispatch_task.is_set());
        // read task is paused
        assert!(io.st().flags.is_read_paused());
        // read buffer is ready
        assert!(io.st().flags.is_read_ready());
        // read backpressure is enabled
        assert!(io.st().flags.is_rd_backpressure());
        assert!(io.is_rd_backpressure());
        assert!(!io.is_wr_backpressure());
        // read task paused
        assert_eq!(lazy(|cx| ctx.poll_read_ready(cx)).await, Poll::Pending);

        // read one byte
        assert_eq!(io.with_read_dst(|buf| buf.split_to(1)), b"1");
        // read buffer is ready
        assert!(io.st().flags.is_read_ready());
        // read backpressure is enabled
        assert!(io.st().flags.is_rd_backpressure());

        // read task is set
        assert!(io.st().read_task.is_set());

        // read one more byte
        assert_eq!(io.with_read_dst(|buf| buf.split_to(1)), b"2");
        // read backpressure is enabled
        assert!(io.st().flags.is_rd_backpressure());

        // dropping below the high watermark does not release backpressure
        assert_eq!(io.with_read_dst(|buf| buf.split_to(1)), b"3");
        assert!(io.st().flags.is_rd_backpressure());
        assert!(io.st().flags.is_read_paused());

        // reaching half of the high watermark releases backpressure
        assert_eq!(io.with_read_dst(|buf| buf.split_to(3)), b"456");
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
        ctx.release_read_buf(BytesMut::copy_from_slice(b"1234"), Poll::Ready(Ok(4)));

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
        assert_eq!(io.with_read_dst(|buf| buf.split_to(4)), b"7890");
        // read backpressure is disabled
        assert!(!io.st().flags.is_rd_backpressure());

        // register dispatcher task
        lazy(|cx| io.poll_dispatch(cx)).await;

        // == No backpressure, 4 bytes in buffer + 3 more
        ctx.release_read_buf(BytesMut::copy_from_slice(b"567"), Poll::Ready(Ok(3)));

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
        assert_eq!(io.with_read_dst(BytesMut::take), b"1234567");
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
            Poll::Ready(Readiness::Close)
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
        ctx.release_read_buf(BytesMut::copy_from_slice(b"1"), Poll::Ready(Ok(1)));

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
        ctx.release_read_buf(BytesMut::copy_from_slice(b"2345678"), Poll::Ready(Ok(7)));
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
        ctx.release_read_buf(BytesMut::copy_from_slice(b"1"), Poll::Ready(Ok(1)));
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
    async fn read_more() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let io = Io::from(server);
        assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

        client.write(TEXT);
        assert_eq!(io.read_more().await.unwrap(), Some(()));
        assert!(matches!(
            lazy(|cx| io.poll_read_more(cx)).await,
            Poll::Ready(Ok(Some(())))
        ));

        let item = io.with_read_dst(BytesMut::take);
        assert_eq!(item, Bytes::from_static(BIN));

        client.write(TEXT);
        sleep(Millis(50)).await;
        assert!(lazy(|cx| io.poll_read_more(cx)).await.is_ready());
        assert!(lazy(|cx| io.poll_read_more(cx)).await.is_ready());
    }

    #[ntex::test]
    async fn read_backpressure() {
        let (client, server) = IoTest::create();

        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(IoConfig::default().set_read_buf(64, 32, 12)),
        );
        assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

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
        assert_eq!(io.read_more().await.unwrap(), Some(()));
    }

    #[ntex::test]
    async fn read_src_releases_read_backpressure() {
        let (client, server) = IoTest::create();

        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(IoConfig::default().set_read_buf(64, 32, 12)),
        );
        assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

        client.write(BIN2);
        client.write(BIN2);
        sleep(Millis(50)).await;
        assert!(io.flags().is_rd_backpressure());

        // On a filterless Io the transport-facing source aliases the
        // application-facing read destination.
        let len = io.get_ref().with_read_src(|buf| {
            let len = buf.len();
            buf.clear();
            len
        });
        assert!(len > 0);
        assert!(!io.flags().is_rd_backpressure());
        assert!(!io.flags().is_read_paused());

        // reads resume
        client.write(BIN2);
        sleep(Millis(50)).await;
        assert!(io.flags().is_read_ready());
    }

    #[ntex::test]
    async fn with_buf_releases_read_backpressure() {
        let (client, server) = IoTest::create();

        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(IoConfig::default().set_read_buf(64, 32, 12)),
        );
        assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());

        client.write(BIN2);
        client.write(BIN2);
        sleep(Millis(50)).await;
        assert!(io.flags().is_rd_backpressure());

        let len = io
            .get_ref()
            .with_buf(|buf| {
                buf.with_read_buffers(|_, dst| {
                    let len = dst.len();
                    dst.clear();
                    len
                })
            })
            .unwrap();
        assert!(len > 0);
        assert!(!io.flags().is_rd_backpressure());
        assert!(!io.flags().is_read_paused());

        // reads resume
        client.write(BIN2);
        sleep(Millis(50)).await;
        assert!(io.flags().is_read_ready());
    }

    #[ntex::test]
    async fn write() {
        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_buf(8)),
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
        io.with_write_src(|buf| buf.put_slice(b"1234")).unwrap();
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
        io.with_write_src(|buf| buf.put_slice(b"5678")).unwrap();
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
        assert_eq!(ctx.with_write_dst(|buf| buf.split_to(4).freeze()), b"1234");
        // continue to write
        assert_eq!(ctx.update_write_status(Ok(4)), IoTaskStatus::Io);
        // write task can proceed
        assert_eq!(
            lazy(|cx| ctx.poll_write_ready(cx)).await,
            Poll::Ready(Readiness::Ready)
        );
        // write task is not paused
        assert!(!io.st().flags.is_write_paused());
        // back-pressure is enabled
        assert!(io.st().flags.is_wr_backpressure());
        // the write buf dropped below the release threshold, back-pressure is
        // released and no further WriteBackpressure is reported
        assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
        // back-pressure is disabled
        assert!(!io.st().flags.is_wr_backpressure());
        assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
        // write buffer is flushed
        assert!(matches!(
            lazy(|cx| io.poll_flush(cx, false)).await,
            Poll::Ready(Ok(()))
        ));

        // full flush write buffer
        io.with_write_src(|buf| buf.put_slice(b"1234")).unwrap();
        assert!(lazy(|cx| io.poll_flush(cx, true)).await.is_pending());
        // full flush is enabled
        assert!(io.st().flags.is_write_flush());
        // back-pressure is enabled
        assert!(io.st().flags.is_wr_backpressure());

        // wrote all data
        Iops::run();
        assert_eq!(ctx.with_write_dst(BytePages::freeze), b"56781234");
        // write task is not paused, so send-buf op is not scheduled
        assert!(!io.st().flags.is_wr_send_scheduled());
        // update status, no more work
        assert_eq!(ctx.update_write_status(Ok(8)), IoTaskStatus::Pause);
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
            Poll::Ready(Readiness::Close)
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
    async fn local_shutdown_reports_peer_gone_without_error() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = Io::from(server);

        // purely local graceful shutdown, the peer does nothing
        io.shutdown().await.unwrap();

        assert!(io.is_closed());
        assert!(matches!(
            lazy(|cx| io.poll_status_update(cx)).await,
            Poll::Ready(IoStatusUpdate::PeerGone(None))
        ));
    }

    #[ntex::test]
    async fn set_config_updates_eager_write_support() {
        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(0)),
        );
        assert!(!io.st().flags.is_direct_wr_enabled());

        // SAFETY: no reference returned by `io.cfg()` is retained.
        unsafe {
            io.set_config(SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(1024)));
        }
        assert!(io.st().flags.is_direct_wr_enabled());
        assert_eq!(io.cfg().write_buf_threshold(), 1024);

        // SAFETY: the previous `io.cfg()` reference was limited to the
        // assertion statement and is no longer live.
        unsafe {
            io.set_config(SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(0)));
        }
        assert!(!io.st().flags.is_direct_wr_enabled());
        assert_eq!(io.cfg().write_buf_threshold(), 0);
    }

    #[ntex::test]
    async fn eager_write_uses_updated_buffer_size() {
        #[derive(Debug)]
        struct DirectWrite;

        impl IoStream for DirectWrite {
            fn start(self, _: IoContext) -> Box<dyn Handle> {
                Box::new(self)
            }
        }

        impl Handle for DirectWrite {
            fn write(&self, ctx: &IoContext) {
                let n = ctx.with_write_dst(|buf| {
                    let n = buf.len();
                    buf.clear();
                    n
                });
                let _ = ctx.update_write_status(Ok(n));
            }
        }

        let io = Io::new(
            DirectWrite,
            SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(1).set_write_buf(8)),
        );

        io.encode_slice(BIN2).unwrap();

        assert_eq!(io.st().buffer.write_buf_size(), 0);
        assert!(io.flags().is_write_paused());
        assert!(!io.flags().is_wr_backpressure());
        assert!(!io.st().flags.is_wr_send_scheduled());
    }

    #[ntex::test]
    async fn eager_write_reports_transport_error() {
        #[derive(Debug)]
        struct FailedWrite;

        impl IoStream for FailedWrite {
            fn start(self, _: IoContext) -> Box<dyn Handle> {
                Box::new(self)
            }
        }

        impl Handle for FailedWrite {
            fn write(&self, ctx: &IoContext) {
                ctx.update_write_status(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "connection reset",
                )));
            }
        }

        let io = Io::new(
            FailedWrite,
            SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(1)),
        );

        let err = io.encode_slice(BIN2).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(err.to_string(), "connection reset");
        assert!(io.is_terminating());
    }

    #[ntex::test]
    async fn terminate_during_eager_write_releases_transport() {
        #[derive(Debug)]
        struct FailedWrite(Rc<Cell<bool>>);

        impl Drop for FailedWrite {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        impl IoStream for FailedWrite {
            fn start(self, _: IoContext) -> Box<dyn Handle> {
                Box::new(self)
            }
        }

        impl Handle for FailedWrite {
            fn write(&self, ctx: &IoContext) {
                ctx.update_write_status(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "connection reset",
                )));
            }
        }

        let dropped = Rc::new(Cell::new(false));
        let io = Io::new(
            FailedWrite(dropped.clone()),
            SharedCfg::new("SRV").add(IoConfig::new().set_write_buf_threshold(1)),
        );

        io.encode_slice(BIN2).unwrap_err();
        assert!(io.is_terminating());

        // the handle is taken for the duration of the direct write, so the
        // terminate it triggered could not release the transport itself
        assert!(dropped.get());
        assert!(io.st().handle.take().is_none());
    }

    #[ntex::test]
    async fn write_backpressure() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);

        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_buf(16)),
        );
        assert!(lazy(|cx| io.poll_read_more(cx)).await.is_pending());
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
        // the write buf drained, back-pressure is released and no status
        // update is reported, same as poll_flush() below
        assert!(lazy(|cx| io.poll_status_update(cx)).await.is_pending());
        assert!(!io.flags().is_wr_backpressure());
        assert!(matches!(
            lazy(|cx| io.poll_flush(cx, false)).await,
            Poll::Ready(Ok(()))
        ));
        assert!(!io.flags().is_wr_backpressure());
    }

    #[ntex::test]
    async fn partial_flush_keeps_write_backpressure_until_half_watermark() {
        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_buf(8)),
        );
        let ctx = IoContext::new(io.get_ref());

        io.encode_slice(b"12345678").unwrap();
        assert!(io.flags().is_wr_backpressure());

        assert_eq!(ctx.with_write_dst(|buf| buf.split_to(1).len()), 1);
        assert_eq!(ctx.update_write_status(Ok(1)), IoTaskStatus::Io);
        assert!(lazy(|cx| io.poll_flush(cx, false)).await.is_pending());
        assert!(io.flags().is_wr_backpressure());

        assert_eq!(ctx.with_write_dst(|buf| buf.split_to(3).len()), 3);
        assert_eq!(ctx.update_write_status(Ok(3)), IoTaskStatus::Io);
        assert!(matches!(
            lazy(|cx| io.poll_flush(cx, false)).await,
            Poll::Ready(Ok(()))
        ));
        assert!(!io.flags().is_wr_backpressure());
    }

    #[ntex::test]
    async fn full_flush_waits_for_inflight_write() {
        // A completion based transport takes ownership of the write pages, so
        // the write buffer empties before the bytes reach the peer. A full
        // flush must not complete until they have.
        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default()),
        );
        let ctx = IoContext::new(io.get_ref());

        io.encode_slice(b"12345678").unwrap();

        let page = ctx.with_write_dst(BytePages::take).unwrap();
        assert_eq!(page.len(), 8);
        // the buffer is empty, the output is in flight
        assert_eq!(io.st().buffer.write_buf_size(), 0);
        assert_eq!(io.st().write_outstanding(), 8);

        assert!(lazy(|cx| io.poll_flush(cx, true)).await.is_pending());

        // the transport reports the write
        assert_eq!(ctx.update_write_status(Ok(page.len())), IoTaskStatus::Pause);
        assert_eq!(io.st().write_outstanding(), 0);
        assert!(matches!(
            lazy(|cx| io.poll_flush(cx, true)).await,
            Poll::Ready(Ok(()))
        ));
    }

    #[ntex::test]
    async fn write_backpressure_counts_inflight_output() {
        // Output owned by the transport is still outstanding, so it keeps
        // back-pressure in place.
        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_buf(8)),
        );
        let ctx = IoContext::new(io.get_ref());

        io.encode_slice(b"12345678").unwrap();
        assert!(io.flags().is_wr_backpressure());

        let page = ctx.with_write_dst(BytePages::take).unwrap();
        assert_eq!(io.st().buffer.write_buf_size(), 0);

        // nothing reached the peer yet, so back-pressure stays enabled
        assert!(lazy(|cx| io.poll_flush(cx, false)).await.is_pending());
        assert!(io.flags().is_wr_backpressure());

        assert_eq!(ctx.update_write_status(Ok(page.len())), IoTaskStatus::Pause);
        assert!(matches!(
            lazy(|cx| io.poll_flush(cx, false)).await,
            Poll::Ready(Ok(()))
        ));
        assert!(!io.flags().is_wr_backpressure());
    }

    #[ntex::test]
    async fn transport_shutdown_waits_for_inflight_write() {
        // The transport shutdown phase reports `Close` once the output has
        // been drained. Output the transport already owns has not been
        // drained until it is reported as written.
        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default()),
        );
        let ctx = IoContext::new(io.get_ref());

        io.encode_slice(b"12345678").unwrap();
        let page = ctx.with_write_dst(BytePages::take).unwrap();

        // enter the transport shutdown phase
        io.st().flags.set_filter_stopping();
        io.st().filters_stopped();
        assert!(io.st().flags.is_stopping());

        // nothing left to submit, but the output is still in flight
        assert_eq!(lazy(|cx| ctx.poll_write_ready(cx)).await, Poll::Pending);

        assert_eq!(ctx.update_write_status(Ok(page.len())), IoTaskStatus::Pause);
        assert_eq!(
            lazy(|cx| ctx.poll_write_ready(cx)).await,
            Poll::Ready(Readiness::Close)
        );
    }

    #[ntex::test]
    async fn shutdown_flushes_write_buf_with_read_backpressure() {
        // Graceful shutdown must flush the pending write buffer even if
        // the peer keeps sending data (read backpressure is enabled).
        let (client, server) = IoTest::create();
        // remote side does not accept any data yet, write task stalls
        client.remote_buffer_cap(0);

        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(
                IoConfig::default()
                    .set_read_buf(8, 4, 16)
                    .set_shutdown_timeout(ntex_util::time::Seconds(2)),
            ),
        );

        // queue response data; remote is stalled so it stays in the write buffer
        io.encode_slice(b"response-tail").unwrap();
        sleep(Millis(50)).await;
        assert_eq!(io.st().buffer.write_buf_size(), 13);

        // peer keeps sending, crossing the read high watermark,
        // read task gets paused with back-pressure enabled
        client.write("0123456789");
        sleep(Millis(50)).await;
        assert!(io.flags().is_read_paused());
        assert!(io.flags().is_rd_backpressure());
        assert_eq!(io.st().buffer.write_buf_size(), 13);

        // start graceful shutdown while the write buffer is not empty
        io.close();
        sleep(Millis(50)).await;

        // peer starts draining the connection
        client.remote_buffer_cap(1024);

        // all previously queued data must be written before io stream is closed.
        // Without the fix graceful shutdown completes immediately (read is paused
        // with back-pressure), dropping the buffered write data, so the read here
        // returns nothing instead of the queued response tail.
        let data = ntex_util::time::timeout(Millis(2000), client.read())
            .await
            .expect("write buffer was dropped during shutdown")
            .unwrap();
        assert_eq!(&data[..], b"response-tail");

        // the connection still closes gracefully afterwards (within the
        // shutdown timeout) instead of hanging
        ntex_util::time::timeout(Millis(4000), io.on_disconnect())
            .await
            .expect("io stream did not disconnect after flush");
    }

    #[ntex::test]
    async fn peer_eof_allows_response_before_shutdown() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = Io::from(server);

        client.write("request");
        client.close().await;

        assert_eq!(
            timeout(Millis(1000), io.recv(&BytesCodec))
                .await
                .expect("request was not decoded")
                .unwrap(),
            Some(Bytes::from_static(b"request"))
        );
        assert!(
            timeout(Millis(1000), io.recv(&BytesCodec))
                .await
                .expect("EOF was not reported")
                .unwrap()
                .is_none()
        );
        assert!(!io.st().flags.is_terminated());

        io.encode(Bytes::from_static(b"response"), &BytesCodec)
            .unwrap();
        timeout(Millis(1000), io.shutdown())
            .await
            .expect("shutdown did not complete")
            .unwrap();

        assert_eq!(
            timeout(Millis(1000), client.read())
                .await
                .expect("response was not flushed")
                .unwrap(),
            b"response"[..]
        );
    }

    #[ntex::test]
    async fn shutdown_waits_for_transport_stop() {
        #[derive(Debug)]
        struct DormantTransport;

        impl IoStream for DormantTransport {
            fn start(self, _: IoContext) -> Box<dyn Handle> {
                Box::new(self)
            }
        }

        impl Handle for DormantTransport {}

        let io = Io::from(DormantTransport);
        let ctx = IoContext::new(io.get_ref());
        let waiter = io.on_disconnect();
        io.st().flags.set_filters_stopped();

        assert!(lazy(|cx| io.poll_shutdown(cx)).await.is_pending());
        assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());

        ctx.stopped(None);
        assert!(matches!(
            lazy(|cx| io.poll_shutdown(cx)).await,
            Poll::Ready(Ok(()))
        ));
        assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_ready());
    }

    #[ntex::test]
    async fn termination_waits_for_transport_stop() {
        #[derive(Debug)]
        struct DormantTransport;

        impl IoStream for DormantTransport {
            fn start(self, _: IoContext) -> Box<dyn Handle> {
                Box::new(self)
            }
        }

        impl Handle for DormantTransport {}

        let io = Io::from(DormantTransport);
        let ctx = IoContext::new(io.get_ref());
        let waiter = io.on_disconnect();
        ctx.stop(Some(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "connection reset",
        )));

        assert!(io.st().flags.is_terminating());
        assert!(!io.st().flags.is_terminated());
        assert!(lazy(|cx| io.poll_shutdown(cx)).await.is_pending());
        assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_pending());

        ctx.stopped(None);
        let Poll::Ready(Err(err)) = lazy(|cx| io.poll_shutdown(cx)).await else {
            panic!("shutdown did not report termination error");
        };
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert!(lazy(|cx| waiter.poll_ready(cx)).await.is_ready());
    }

    #[ntex::test]
    async fn send_buf_retains_termination_error() {
        #[derive(Debug)]
        struct DormantTransport;

        impl IoStream for DormantTransport {
            fn start(self, _: IoContext) -> Box<dyn Handle> {
                Box::new(self)
            }
        }

        impl Handle for DormantTransport {}

        let io = Io::from(DormantTransport);
        let ctx = IoContext::new(io.get_ref());
        ctx.stop(Some(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "connection reset",
        )));

        let err = io.get_ref().send_buf().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(err.to_string(), "connection reset");

        ctx.stopped(None);
        let err = io.shutdown().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(err.to_string(), "connection reset");
    }

    #[ntex::test]
    async fn read_readiness_reports_termination_error() {
        #[derive(Debug)]
        struct DormantTransport;

        impl IoStream for DormantTransport {
            fn start(self, _: IoContext) -> Box<dyn Handle> {
                Box::new(self)
            }
        }

        impl Handle for DormantTransport {}

        let io = Io::from(DormantTransport);
        let ctx = IoContext::new(io.get_ref());
        ctx.stop(Some(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "connection reset",
        )));

        let Poll::Ready(Err(err)) = lazy(|cx| io.poll_read_more(cx)).await else {
            panic!("read request did not report termination error");
        };
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(err.to_string(), "connection reset");

        let Poll::Ready(Err(err)) = lazy(|cx| io.poll_read_notify(cx)).await else {
            panic!("read notification did not report termination error");
        };
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(err.to_string(), "connection reset");

        let err = io.read_exact(&mut [0]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(err.to_string(), "connection reset");
    }

    #[ntex::test]
    async fn zero_shutdown_timeout_does_not_force_filter_shutdown() {
        #[derive(Debug)]
        struct PendingShutdown(Rc<Cell<bool>>);

        impl FilterLayer for PendingShutdown {
            fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }

            fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }

            fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
                if self.0.get() {
                    Ok(Poll::Ready(()))
                } else {
                    Ok(Poll::Pending)
                }
            }
        }

        let (_client, server) = IoTest::create();
        let ready = Rc::new(Cell::new(false));
        let io = Io::new(
            server,
            SharedCfg::new("SRV")
                .add(IoConfig::default().set_shutdown_timeout(ntex_util::time::Seconds::ZERO)),
        )
        .add_filter(PendingShutdown(ready.clone()));

        io.close();
        sleep(Millis(50)).await;
        assert!(!io.st().flags.is_terminated());
    }

    #[ntex::test]
    async fn intermediate_filter_output_reaches_transport() {
        // A filter that emits output while processing reads, the way a TLS
        // layer emits handshake records.
        #[derive(Debug)]
        struct Emit;

        impl FilterLayer for Emit {
            fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
                buf.with_read_buffers(|src, dst| {
                    if let Some(src) = src {
                        dst.extend_from_slice(src);
                        src.clear();
                    }
                });
                buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"pong"));
                Ok(())
            }

            fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
                buf.with_write_buffers(BytePages::move_to);
                Ok(())
            }

            fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
                Ok(Poll::Ready(()))
            }
        }

        #[derive(Debug)]
        struct Passthrough;

        impl FilterLayer for Passthrough {
            fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
                buf.with_read_buffers(|src, dst| {
                    if let Some(src) = src {
                        dst.extend_from_slice(src);
                        src.clear();
                    }
                });
                Ok(())
            }

            fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
                buf.with_write_buffers(BytePages::move_to);
                Ok(())
            }

            fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
                Ok(Poll::Ready(()))
            }
        }

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        // Two layers, so the bytes `Emit` writes during read processing land in
        // an intermediate buffer that `Stack::write_buf_size()` does not count.
        // Only the forced write-chain pass moves them out to the transport.
        let io = Io::from(server).add_filter(Passthrough).add_filter(Emit);

        client.write("ping");
        let _ = io.recv(&BytesCodec).await.unwrap();
        sleep(Millis(50)).await;
        assert!(client.read_any().starts_with(b"pong"));
    }

    #[ntex::test]
    async fn peer_eof_completes_filter_shutdown() {
        #[derive(Debug)]
        struct PendingShutdown;

        impl FilterLayer for PendingShutdown {
            fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }

            fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
                buf.with_write_buffers(BytePages::move_to);
                Ok(())
            }

            fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
                // waits for input that can never arrive after a clean eof
                buf.with_write_buffers(BytePages::move_to);
                Ok(Poll::Pending)
            }
        }

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = Io::new(
            server,
            SharedCfg::new("SRV")
                .add(IoConfig::default().set_shutdown_timeout(ntex_util::time::Seconds(30))),
        )
        .add_filter(PendingShutdown);

        io.encode_slice(b"bye").unwrap();

        // peer closes cleanly, no further input can arrive
        let peer = client.clone();
        drop(client);
        assert!(io.read_more().await.unwrap().is_none());
        assert!(io.st().flags.is_read_eof());

        // the shutdown completes without waiting for the shutdown timeout
        timeout(Millis(1000), io.shutdown())
            .await
            .expect("transport shutdown did not complete")
            .unwrap();
        assert!(io.st().flags.is_terminated());

        // buffered output still reached the peer
        assert_eq!(peer.read_any(), Bytes::from_static(b"bye"));
    }

    /// A filter that forwards input and never finishes its own shutdown.
    #[derive(Debug)]
    struct StuckShutdown(Cell<bool>);

    impl FilterLayer for StuckShutdown {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_read_buffers(|src, dst| {
                if let Some(src) = src {
                    dst.extend_from_slice(src);
                    src.clear();
                }
            });
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }

        fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
            self.0.set(true);
            Ok(Poll::Pending)
        }
    }

    #[ntex::test]
    async fn transport_shutdown_pauses_read_task() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = Io::new(server, SharedCfg::new("SRV"));

        client.write("before");
        sleep(Millis(25)).await;
        assert_eq!(
            io.recv(&BytesCodec).await.unwrap().unwrap(),
            b"before".as_ref()
        );

        // stall the output so the transport shutdown phase does not complete
        client.remote_buffer_cap(0);
        io.get_ref().with_write_dst(|b| b.extend_from_slice(b"out"));

        // enter the transport shutdown phase
        io.st().flags.set_filter_stopping();
        io.st().flags.set_filters_stopped();
        io.st().wake_read_task();

        // the filters are done, so input is left in the transport; it is
        // discarded by the transport itself right before it closes
        client.write("after");
        sleep(Millis(50)).await;
        assert!(!io.st().flags.is_terminated());
        assert_eq!(client.remote_buffer(|buf| buf.len()), 5);
    }

    #[ntex::test]
    async fn filter_shutdown_applies_read_backpressure() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024 * 1024);
        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(
                IoConfig::default()
                    .set_read_buf(1024, 256, 8)
                    .set_shutdown_timeout(ntex_util::time::Seconds(30)),
            ),
        )
        .add_filter(StuckShutdown(Cell::new(false)));

        let ioref = io.get_ref();
        let high = 1024;
        ntex::rt::spawn(async move {
            let _ = io.shutdown().await;
        });
        sleep(Millis(50)).await;

        // The filter never completes, so the connection stays in the filter
        // shutdown phase while the peer keeps sending.
        for _ in 0..40 {
            client.write("A".repeat(1024));
            sleep(Millis(5)).await;
        }
        sleep(Millis(100)).await;

        // Reads are backpressured instead of draining the peer without bound.
        let buffered = ioref.with_read_dst(|buf| buf.len());
        assert!(
            buffered <= high * 2,
            "read buffer grew to {buffered} with a high watermark of {high}"
        );
        assert!(
            client.remote_buffer(|buf| !buf.is_empty()),
            "peer send buffer was drained despite read backpressure"
        );
    }

    #[ntex::test]
    async fn filter_shutdown_blocked_by_unconsumed_input() {
        // The dispatcher stops consuming with a full read buffer, so the filter
        // cannot receive the input it is waiting for. The shutdown gives up on
        // the filter handshake promptly instead of reading without bound until
        // the deadline.
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024 * 1024);
        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(
                IoConfig::default()
                    .set_read_buf(1024, 256, 8)
                    .set_shutdown_timeout(ntex_util::time::Seconds(30)),
            ),
        )
        .add_filter(StuckShutdown(Cell::new(false)));

        client.write("A".repeat(4096));
        sleep(Millis(50)).await;
        assert!(
            io.get_ref().is_rd_backpressure(),
            "read backpressure was not active before the shutdown"
        );

        // Completes well inside the 30 second deadline, so it is the blocked
        // detection that ends the phase rather than the timeout.
        let err = timeout(Millis(3000), io.shutdown())
            .await
            .expect("shutdown did not complete")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[ntex::test]
    async fn filter_shutdown_timeout_is_reported() {
        #[derive(Debug)]
        struct PendingShutdown;

        impl FilterLayer for PendingShutdown {
            fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }

            fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }

            fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
                Ok(Poll::Pending)
            }
        }

        let (_client, server) = IoTest::create();
        let io = Io::new(
            server,
            SharedCfg::new("SRV")
                .add(IoConfig::default().set_shutdown_timeout(ntex_util::time::Seconds(1))),
        )
        .add_filter(PendingShutdown);

        let err = timeout(Millis(3000), io.shutdown())
            .await
            .expect("transport shutdown did not complete")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(io.st().flags.is_terminated());
        assert!(!io.st().flags.is_terminating());
    }

    #[ntex::test]
    async fn blocked_filter_shutdown_flushes_buffered_output() {
        #[derive(Debug)]
        struct ClosingShutdown(Cell<bool>);

        impl FilterLayer for ClosingShutdown {
            fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }

            fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
                buf.with_write_buffers(BytePages::move_to);
                Ok(())
            }

            fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
                // emit a closing record, like a tls close_notify
                if !self.0.replace(true) {
                    buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"bye"));
                }
                Ok(Poll::Pending)
            }
        }

        let (client, server) = IoTest::create();
        // the peer cannot accept the closing record yet
        client.remote_buffer_cap(0);

        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(
                IoConfig::default()
                    .set_read_buf(8, 4, 16)
                    .set_shutdown_timeout(ntex_util::time::Seconds(10)),
            ),
        )
        .add_filter(ClosingShutdown(Cell::new(false)));

        io.st().flags.set_read_ready_and_backpressure();
        io.close();
        sleep(Millis(50)).await;

        // the filter shutdown is blocked, so the transport shutdown phase
        // starts right away and takes over draining the closing record
        assert!(io.st().flags.is_stopping());

        // let the peer accept the buffered bytes
        client.remote_buffer_cap(1024);
        assert_eq!(
            timeout(Millis(1000), client.read())
                .await
                .expect("closing record was not written")
                .unwrap(),
            Bytes::from_static(b"bye")
        );

        let err = timeout(Millis(1000), io.shutdown())
            .await
            .expect("transport shutdown did not complete")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[ntex::test]
    async fn one_deadline_bounds_both_shutdown_phases() {
        #[derive(Debug)]
        struct ClosingShutdown(Cell<bool>);

        impl FilterLayer for ClosingShutdown {
            fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }

            fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
                buf.with_write_buffers(BytePages::move_to);
                Ok(())
            }

            fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
                if !self.0.replace(true) {
                    buf.with_write_buffers(|_, dst| dst.extend_from_slice(b"bye"));
                }
                Ok(Poll::Pending)
            }
        }

        let (client, server) = IoTest::create();
        // the peer never accepts the closing record, so neither the filter
        // shutdown nor the transport drain can ever complete
        client.remote_buffer_cap(0);

        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(
                IoConfig::default()
                    .set_read_buf(8, 4, 16)
                    .set_shutdown_timeout(ntex_util::time::Seconds(1)),
            ),
        )
        .add_filter(ClosingShutdown(Cell::new(false)));

        let start = std::time::Instant::now();
        io.close();

        timeout(Millis(5000), io.shutdown())
            .await
            .expect("transport shutdown did not complete")
            .unwrap_err();
        assert!(io.st().flags.is_terminated());

        // both phases stall, yet a single shutdown timeout covers them: a
        // per-phase deadline would take twice as long
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(1600),
            "shutdown took {elapsed:?}, the deadline did not span both phases"
        );
    }

    #[ntex::test]
    async fn blocked_filter_shutdown_is_reported() {
        #[derive(Debug)]
        struct PendingShutdown;

        impl FilterLayer for PendingShutdown {
            fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }

            fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }

            fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
                Ok(Poll::Pending)
            }
        }

        let (_client, server) = IoTest::create();
        let io = Io::new(
            server,
            SharedCfg::new("SRV").add(
                IoConfig::default()
                    .set_read_buf(8, 4, 16)
                    .set_shutdown_timeout(ntex_util::time::Seconds(10)),
            ),
        )
        .add_filter(PendingShutdown);

        io.st().flags.set_read_ready_and_backpressure();
        io.close();
        sleep(Millis(50)).await;

        let err = timeout(Millis(1000), io.shutdown())
            .await
            .expect("transport shutdown did not complete")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(io.st().flags.is_terminated());
        assert!(!io.st().flags.is_terminating());
    }

    #[ntex::test]
    async fn shutdown() {
        // layer drops all unprocessed data after filter shutdown
        #[derive(Debug)]
        struct F;

        impl FilterLayer for F {
            fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }
            fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
                Ok(())
            }
        }

        let io = Io::new(
            IoTest::create().0,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_buf(8)),
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
        let err = io.with_write_src(|_| 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);

        let io = io.add_filter(F);
        let layer = Layer::new(F, Base::new(io.get_ref()));

        let st = io.st();
        st.buffer.with_write_src(|p| p.put_slice(b"123"));
        assert_eq!(st.buffer.write_buf_size(), 3);
        let res = st.buffer.with_filter(io.as_ref(), |f| layer.shutdown(f));
        assert!(matches!(res, Ok(Poll::Ready(()))));
        assert_eq!(st.buffer.write_buf_size(), 0);

        // == terminate
        ctx.stop(None);
        assert!(st.flags.is_closed());
        assert!(st.flags.is_terminating());
        assert!(!st.flags.is_terminated());
        assert!(st.flags.is_stopping_filters());

        let err = io.with_write_src(|_| 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);

        ctx.stopped(None);
        assert!(st.flags.is_terminated());
    }

    struct FixedSize(usize);

    impl Decoder for FixedSize {
        type Item = Bytes;
        type Error = io::Error;

        fn decode(&self, src: &mut BytesMut) -> Result<Option<Bytes>, io::Error> {
            if src.len() < self.0 {
                Ok(None)
            } else {
                Ok(Some(src.split_to(self.0)))
            }
        }
    }

    #[ntex::test]
    async fn recv_reports_truncated_stream() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = Io::new(server, SharedCfg::new("SRV"));

        // a partial item, then the peer goes away
        client.write("123");
        sleep(Millis(25)).await;
        client.close().await;

        let err = io.recv(&FixedSize(8)).await.err().unwrap();
        let Either::Right(err) = err else {
            panic!("expected a transport error")
        };
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        // the partial item is still buffered, it is not discarded
        assert_eq!(io.with_read_dst(|b| b.len()), 3);
    }

    #[ntex::test]
    async fn recv_reports_clean_eof() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = Io::new(server, SharedCfg::new("SRV"));

        // a whole item, then the peer goes away
        client.write("12345678");
        sleep(Millis(25)).await;
        client.close().await;

        assert_eq!(io.recv(&FixedSize(8)).await.unwrap().unwrap(), "12345678");
        assert!(io.recv(&FixedSize(8)).await.unwrap().is_none());
    }

    #[ntex::test]
    async fn recv_local_shutdown_is_not_truncation() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = Io::new(server, SharedCfg::new("SRV"));

        // undecodable input is left buffered, but the peer never closed and
        // the shutdown is started locally, so this is not a truncated stream
        client.write("123");
        sleep(Millis(25)).await;
        io.close();
        sleep(Millis(25)).await;

        assert!(io.recv(&FixedSize(8)).await.unwrap().is_none());
        assert_eq!(io.with_read_dst(|b| b.len()), 3);
    }
}
