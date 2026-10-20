use std::cell::{Cell, UnsafeCell};
use std::{fmt, hash, io, marker, mem, ops, ptr, rc::Rc, sync::Arc};

use ntex_bytes::BytePageSize;
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_util::{task::LocalWaker, time::Sleep};

use crate::buf::Stack;
use crate::cfg::IoConfig;
use crate::ctx::IoContext;
use crate::filter::{Base, Filter, Layer};
use crate::filterptr::FilterPtr;
use crate::flags::Flags;
use crate::metrics::{CloseReason, IoMetrics};
use crate::ops::{Id, IoManager, TimerHandle};
use crate::seal::{IoBoxed, Sealed};
use crate::utils::Extensions;
use crate::{FilterLayer, Handle, IoStream};

mod state;
mod stream;
#[cfg(test)]
mod tests;

/// Read batches in a row small enough for a smaller page before the read page
/// size shrinks.
const RD_SHRINK_BATCHES: u8 = 4;

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
    ///
    /// The count saturates at `u32::MAX`. More than 4 GiB in flight is not
    /// tracked exactly, see [`Io::poll_flush`].
    pub(super) wr_inflight: Cell<u32>,
    /// Page size of new read buffers.
    ///
    /// It adapts to the input between the configured min and max read sizes,
    /// see [`IoState::track_read`].
    rd_size: Cell<BytePageSize>,
    /// Input read since the last read that did not fill its buffer.
    rd_batch: Cell<u32>,
    /// Consecutive read batches small enough for a smaller page.
    rd_small: Cell<u8>,
    /// Per-connection read buffer tracker, created on first use, see
    /// [`IoState::buf_tracker`].
    #[cfg(feature = "tracker")]
    rd_tracker: UnsafeCell<Option<ntex_bytes::BytesTracker>>,
    pub(super) extensions: Extensions,
    metrics: UnsafeCell<Arc<IoMetrics>>,
    /// Read buffer size last reported to `metrics`, saturated at `u32::MAX`.
    rd_buffered: Cell<u32>,
    /// Read buffer capacity last reported to `metrics`, saturated at
    /// `u32::MAX`.
    rd_capacity: Cell<u32>,
    /// Write buffer size last reported to `metrics`, saturated at `u32::MAX`.
    wr_buffered: Cell<u32>,
    pub(super) close: Cell<Close>,
}

/// Close reason of a connection, counted in `metrics` on drop.
#[derive(Copy, Clone, Debug)]
pub(super) enum Close {
    Unset,
    Reason(CloseReason),
    /// Placeholder state of a taken `Io`, not a connection.
    Uncounted,
}

impl Io {
    /// Creates a new `Io` instance.
    pub fn new<I: IoStream, T: Into<SharedCfg>>(stream: I, cfg: T) -> Self {
        let cfg = cfg.into().get::<IoConfig>();
        let size = cfg.write_size();
        let flags = Flags::new(cfg.write_buf_threshold() > 0);
        let metrics = cfg.metrics();

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
            rd_size: Cell::new(BytePageSize::Size4),
            rd_batch: Cell::new(0),
            rd_small: Cell::new(0),
            #[cfg(feature = "tracker")]
            rd_tracker: UnsafeCell::new(None),
            extensions: Extensions::default(),
            metrics: UnsafeCell::new(metrics),
            rd_buffered: Cell::new(0),
            rd_capacity: Cell::new(0),
            wr_buffered: Cell::new(0),
            close: Cell::new(Close::Unset),
        });
        inner.reset_read_size();
        inner.filter.set(Base::new(IoRef(inner.clone())));

        let ioref = IoRef(inner);
        ioref.0.id.set(IoManager::register(&ioref));

        // `Io` owns the state before the stream starts, so that a panic in
        // `IoStream::start` drops the filter and unregisters the connection
        let io = Io(UnsafeCell::new(ioref), marker::PhantomData);

        // start io tasks
        let hnd = stream.start(IoContext::new(io.io_ref().clone()));
        io.st().handle.set(Some(hnd));
        io
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
            rd_size: Cell::new(BytePageSize::Size4),
            rd_batch: Cell::new(0),
            rd_small: Cell::new(0),
            #[cfg(feature = "tracker")]
            rd_tracker: UnsafeCell::new(None),
            extensions: Extensions::default(),
            metrics: UnsafeCell::new(IoMetrics::detached()),
            rd_buffered: Cell::new(0),
            rd_capacity: Cell::new(0),
            wr_buffered: Cell::new(0),
            close: Cell::new(Close::Uncounted),
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
    ///
    /// # Safety
    ///
    /// No reference derived from `self` may be alive across this call, the
    /// filter returned by [`Io::filter`], the `IoRef` it dereferences to and
    /// the config returned by [`IoRef::cfg`] all borrow the transferred state,
    /// which is dropped together with the returned `Io`.
    pub unsafe fn take(&self) -> Self {
        Self(UnsafeCell::new(self.take_io_ref()), marker::PhantomData)
    }

    fn take_io_ref(&self) -> IoRef {
        unsafe { mem::replace(&mut *self.0.get(), IoRef::create_empty()) }
    }

    /// Moves the shared state out of `self` without a placeholder, `Drop`
    /// does not run for `self`.
    fn into_io_ref(self) -> IoRef {
        let this = mem::ManuallyDrop::new(self);
        // SAFETY: `this` is not used or dropped afterwards
        unsafe { ptr::read(this.0.get()) }
    }

    /// Panics if the filter chain is in use, it cannot be changed then.
    #[track_caller]
    fn check_not_borrowed(&self) {
        if self.st().buffer.is_borrowed() {
            let tag = self.tag();
            // the state is leaked, dropping `self` while unwinding would
            // drop the filter that is in use
            mem::forget(self.take_io_ref());
            panic!("{tag}: filter chain is changed while it is in use");
        }
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
    /// immediately, the read page size restarts at the new min read size.
    /// Existing allocated buffers and an already registered timer are not
    /// recreated.
    ///
    /// With the `tracker` feature, the read buffers held by the connection
    /// are charged to the tracker of the new `BufTracker` mode, a per-connection
    /// tracker is kept if both configs use one. A read buffer taken by the
    /// transport for an in-flight read keeps its charge, buffers stay
    /// charged to the old tracker if the new mode is disabled.
    ///
    /// # Safety
    ///
    /// No reference obtained from [`IoRef::cfg`], `IoRef::buf_tracker`
    /// or `IoContext::buf_tracker` for this connection may be live when
    /// this method is called or used afterward. Replacing the configuration
    /// may release the allocation backing those references.
    pub unsafe fn set_config<T: Into<SharedCfg>>(&self, cfg: T) {
        let cfg = cfg.into().get::<IoConfig>();
        let page_size = cfg.write_size();
        if self.cfg().write_size() != page_size {
            self.st().buffer.set_page_size(page_size);
        }
        self.st()
            .flags
            .set_direct_wr_enabled(cfg.write_buf_threshold() > 0);
        let st = self.st();
        let metrics = cfg.metrics();
        if !ptr::eq(st.metrics(), Arc::as_ptr(&metrics)) {
            st.metrics().move_to(
                &metrics,
                st.flags.is_rd_backpressure(),
                st.flags.is_wr_backpressure(),
                st.rd_buffered.get(),
                st.rd_capacity.get(),
                st.wr_buffered.get(),
            );
            // SAFETY: the caller guarantees no reference into the state is live
            unsafe {
                *st.metrics.get() = metrics;
            }
        }
        #[cfg(feature = "tracker")]
        st.update_buf_tracker(&cfg);
        unsafe {
            st.cfg.replace(cfg);
        }
        st.reset_read_size();
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
    ///
    /// # Panics
    ///
    /// Panics if called while the connection's filters or buffers are in use,
    /// for example from a closure, codec or filter invoked by this connection.
    pub fn seal(self) -> Io<Sealed> {
        self.check_not_borrowed();
        let state = self.into_io_ref();
        state.0.filter.seal::<F>();

        Io(UnsafeCell::new(state), marker::PhantomData)
    }

    #[inline]
    /// Converts the current I/O stream into a boxed version.
    pub fn boxed(self) -> IoBoxed {
        IoBoxed(self.seal())
    }

    #[inline]
    /// Adds a new processing layer to the current filter chain.
    ///
    /// # Panics
    ///
    /// Panics if called while the connection's filters or buffers are in use,
    /// for example from a closure, codec or filter invoked by this connection.
    pub fn add_filter<U>(self, nf: U) -> Io<Layer<U, F>>
    where
        U: FilterLayer,
    {
        self.check_not_borrowed();
        self.with_callbacks(|cb| cb.before_processing(&self));

        // Write buffer processing may be delayed,
        // call the filter chain to process pending writes
        if let Err(e) = self.st().buffer.process_write_buf_no_cb(&self) {
            self.fail_filters(e);
        }

        let state = self.into_io_ref();

        // Add the buffers layer
        state.0.buffer.add_layer(state.0.cfg.write_size());

        // Replace current filter
        state.0.filter.add_filter::<F, U>(nf);

        let io = Io(UnsafeCell::new(state), marker::PhantomData);

        // push read data into new filter
        if let Err(e) = io.st().buffer.process_read_buf_no_cb(&io) {
            io.fail_filters(e);
        }
        io.with_callbacks(|cb| cb.after_processing(&io));

        io
    }

    #[allow(clippy::items_after_statements)]
    /// Wraps the current layer with a wrapper.
    ///
    /// # Panics
    ///
    /// Panics if called while the connection's filters or buffers are in use,
    /// for example from a closure, codec or filter invoked by this connection.
    pub fn map_filter<U, R>(self, f: U) -> Io<R>
    where
        U: FnOnce(F) -> R,
        R: Filter,
    {
        self.check_not_borrowed();
        self.with_callbacks(|cb| cb.before_processing(&self));

        // Write buffer processing may be delayed,
        // call the filter chain to process pending writes
        if let Err(e) = self.st().buffer.process_write_buf(&self) {
            self.fail_filters(e);
        }

        // `f` owns the filter, if it unwinds the filter chain is gone and the
        // connection must be torn down here, `Drop for Io` skips it without
        // a filter
        struct Guard<'a>(&'a IoRef);

        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                let st = &self.0.0;
                st.force_close_connection();
                st.buffer.release();
                st.sync_read_buffered();
                drop(st.extensions.take_callbacks());
            }
        }

        let guard = Guard(self.io_ref());
        self.st().filter.map_filter::<F, U, R>(f);
        mem::forget(guard);

        let state = self.into_io_ref();
        let io = Io(UnsafeCell::new(state), marker::PhantomData);
        io.with_callbacks(|cb| cb.after_processing(&io));
        io
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

        // code run by the filter chain dropped the `Io`, the filter is in use
        let in_use = st.filter.is_set() && st.buffer.is_borrowed();

        if st.filter.is_set() {
            if in_use {
                // the filter cannot be dropped, it is leaked
                st.force_close_connection();
                st.filter.leak();
            } else {
                // filter is unsafe and must be dropped explicitly,
                // and won't be dropped without special attention
                if !st.flags.is_closed() {
                    log::trace!("{}: Io is dropped, terminate connection", st.tag());
                }

                if st.write_outstanding() == 0 {
                    // Everything the application wrote has reached the transport,
                    // so the connection can end with a normal FIN and the peer
                    // sees a clean end of stream.
                    st.terminate_connection(None);
                } else {
                    // Output is still buffered and the filter chain is about to go
                    // away, so it can never be delivered. Abort instead, so that
                    // the peer cannot mistake a truncated stream for a complete
                    // one.
                    st.force_close_connection();
                }
                st.filter.drop_filter::<F>();
            }

            // Nothing can consume buffered input or deliver buffered output
            // anymore, but the state may outlive the `Io` for a while, held by
            // the transport while it closes or by other `IoRef` handles.
            st.buffer.release();
            st.sync_read_buffered();

            // Callbacks may hold an `IoRef` to this connection, which would keep
            // the state alive through a reference cycle. They are dropped outside
            // the extensions borrow, because their destructor may use the `IoRef`.
            drop(st.extensions.take_callbacks());
        }

        IoManager::unregister(self.io_ref());

        // a panic while unwinding would abort
        assert!(
            !in_use || std::thread::panicking(),
            "{}: Io is dropped while its filter is in use",
            st.tag()
        );
    }
}
