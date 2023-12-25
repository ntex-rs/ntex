use std::cell::Cell;
use std::task::{Context, Poll};
use std::{fmt, future::Future, hash, io, marker, mem, ops, pin::Pin, ptr, rc::Rc};

use ntex_bytes::{PoolId, PoolRef};
use ntex_codec::{Decoder, Encoder};
use ntex_util::{future::poll_fn, future::Either, task::LocalWaker, time::Seconds};

use crate::buf::Stack;
use crate::filter::{Base, Filter, Layer, NullFilter};
use crate::seal::Sealed;
use crate::tasks::{ReadContext, WriteContext};
use crate::timer::TimerHandle;
use crate::{Decoded, FilterLayer, Handle, IoStatusUpdate, IoStream, RecvError};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub struct Flags: u16 {
        /// io is closed
        const IO_STOPPED          = 0b0000_0000_0000_0001;
        /// shutdown io tasks
        const IO_STOPPING         = 0b0000_0000_0000_0010;
        /// shuting down filters
        const IO_STOPPING_FILTERS = 0b0000_0000_0000_0100;
        /// initiate filters shutdown timeout in write task
        const IO_FILTERS_TIMEOUT  = 0b0000_0000_0000_1000;

        /// pause io read
        const RD_PAUSED           = 0b0000_0000_0001_0000;
        /// new data is available
        const RD_READY            = 0b0000_0000_0010_0000;
        /// read buffer is full
        const RD_BUF_FULL         = 0b0000_0000_0100_0000;
        /// any new data is available
        const RD_FORCE_READY      = 0b0000_0000_1000_0000;

        /// wait write completion
        const WR_WAIT             = 0b0000_0001_0000_0000;
        /// write buffer is full
        const WR_BACKPRESSURE     = 0b0000_0010_0000_0000;
        /// write task paused
        const WR_PAUSED           = 0b0000_0100_0000_0000;

        /// dispatcher is marked stopped
        const DSP_STOP            = 0b0001_0000_0000_0000;
        /// timeout occured
        const DSP_TIMEOUT         = 0b0010_0000_0000_0000;
    }
}

/// Interface object to underlying io stream
pub struct Io<F = Base>(pub(super) IoRef, FilterItem<F>);

#[derive(Clone)]
pub struct IoRef(pub(super) Rc<IoState>);

pub(crate) struct IoState {
    pub(super) flags: Cell<Flags>,
    pub(super) pool: Cell<PoolRef>,
    pub(super) disconnect_timeout: Cell<Seconds>,
    pub(super) error: Cell<Option<io::Error>>,
    pub(super) read_task: LocalWaker,
    pub(super) write_task: LocalWaker,
    pub(super) dispatch_task: LocalWaker,
    pub(super) buffer: Stack,
    pub(super) filter: Cell<&'static dyn Filter>,
    pub(super) handle: Cell<Option<Box<dyn Handle>>>,
    pub(super) timeout: Cell<TimerHandle>,
    pub(super) tag: Cell<&'static str>,
    #[allow(clippy::box_collection)]
    pub(super) on_disconnect: Cell<Option<Box<Vec<LocalWaker>>>>,
}

const DEFAULT_TAG: &str = "IO";

impl IoState {
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
        log::trace!("{}: Timeout, notify dispatcher", self.tag.get());

        let mut flags = self.flags.get();
        if !flags.contains(Flags::DSP_TIMEOUT) {
            flags.insert(Flags::DSP_TIMEOUT);
            self.flags.set(flags);
            self.dispatch_task.wake();
        }
    }

    pub(super) fn notify_disconnect(&self) {
        if let Some(on_disconnect) = self.on_disconnect.take() {
            for item in on_disconnect.into_iter() {
                item.wake();
            }
        }
    }

    pub(super) fn io_stopped(&self, err: Option<io::Error>) {
        if err.is_some() {
            self.error.set(err);
        }
        self.read_task.wake();
        self.write_task.wake();
        self.dispatch_task.wake();
        self.notify_disconnect();
        self.handle.take();
        self.insert_flags(
            Flags::IO_STOPPED | Flags::IO_STOPPING | Flags::IO_STOPPING_FILTERS,
        );
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
                self.tag.get(),
                self.flags.get()
            );
            self.insert_flags(Flags::IO_STOPPING_FILTERS);
            self.read_task.wake();
        }
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
        (self as *const _ as usize).hash(state);
    }
}

impl Drop for IoState {
    #[inline]
    fn drop(&mut self) {
        self.buffer.release(self.pool.get());
    }
}

impl fmt::Debug for IoState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let err = self.error.take();
        let res = f
            .debug_struct("IoState")
            .field("flags", &self.flags)
            .field("pool", &self.pool)
            .field("disconnect_timeout", &self.disconnect_timeout)
            .field("timeout", &self.timeout)
            .field("error", &err)
            .field("buffer", &self.buffer)
            .finish();
        self.error.set(err);
        res
    }
}

impl Io {
    #[inline]
    /// Create `Io` instance
    pub fn new<I: IoStream>(io: I) -> Self {
        Self::with_memory_pool(io, PoolId::DEFAULT.pool_ref())
    }

    #[inline]
    /// Create `Io` instance in specific memory pool.
    pub fn with_memory_pool<I: IoStream>(io: I, pool: PoolRef) -> Self {
        let inner = Rc::new(IoState {
            pool: Cell::new(pool),
            flags: Cell::new(Flags::empty()),
            error: Cell::new(None),
            disconnect_timeout: Cell::new(Seconds(1)),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            buffer: Stack::new(),
            filter: Cell::new(NullFilter::get()),
            handle: Cell::new(None),
            timeout: Cell::new(TimerHandle::default()),
            on_disconnect: Cell::new(None),
            tag: Cell::new(DEFAULT_TAG),
        });

        let filter = Box::new(Base::new(IoRef(inner.clone())));
        let filter_ref: &'static dyn Filter = unsafe {
            let filter: &dyn Filter = filter.as_ref();
            std::mem::transmute(filter)
        };
        inner.filter.replace(filter_ref);

        let io_ref = IoRef(inner);

        // start io tasks
        let hnd = io.start(ReadContext::new(&io_ref), WriteContext::new(&io_ref));
        io_ref.0.handle.set(hnd);

        Io(io_ref, FilterItem::with_filter(filter))
    }
}

impl<F> Io<F> {
    #[inline]
    /// Set memory pool
    pub fn set_memory_pool(&self, pool: PoolRef) {
        self.0 .0.buffer.set_memory_pool(pool);
        self.0 .0.pool.set(pool);
    }

    #[inline]
    /// Set io disconnect timeout in millis
    pub fn set_disconnect_timeout(&self, timeout: Seconds) {
        self.0 .0.disconnect_timeout.set(timeout);
    }

    #[inline]
    /// Clone current io object.
    ///
    /// Current io object becomes closed.
    pub fn take(&mut self) -> Self {
        let inner = Rc::new(IoState {
            pool: self.0 .0.pool.clone(),
            flags: Cell::new(
                Flags::DSP_STOP
                    | Flags::IO_STOPPED
                    | Flags::IO_STOPPING
                    | Flags::IO_STOPPING_FILTERS,
            ),
            error: Cell::new(None),
            disconnect_timeout: Cell::new(Seconds(1)),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            buffer: Stack::new(),
            filter: Cell::new(NullFilter::get()),
            handle: Cell::new(None),
            timeout: Cell::new(TimerHandle::default()),
            on_disconnect: Cell::new(None),
            tag: Cell::new(DEFAULT_TAG),
        });

        let state = mem::replace(&mut self.0, IoRef(inner));
        let filter = mem::replace(&mut self.1, FilterItem::null());
        Self(state, filter)
    }
}

impl<F> Io<F> {
    #[inline]
    #[doc(hidden)]
    /// Get current state flags
    pub fn flags(&self) -> Flags {
        self.0 .0.flags.get()
    }

    #[inline]
    /// Get instance of `IoRef`
    pub fn get_ref(&self) -> IoRef {
        self.0.clone()
    }

    /// Get current io error
    fn error(&self) -> Option<io::Error> {
        self.0 .0.error.take()
    }
}

impl<F: FilterLayer, T: Filter> Io<Layer<F, T>> {
    #[inline]
    /// Get referece to a filter
    pub fn filter(&self) -> &F {
        &self.1.filter().0
    }
}

impl<F: Filter> Io<F> {
    #[inline]
    /// Convert current io stream into sealed version
    pub fn seal(mut self) -> Io<Sealed> {
        let (filter, filter_ref) = self.1.seal();
        self.0 .0.filter.replace(filter_ref);
        Io(self.0.clone(), filter)
    }

    #[inline]
    /// Map current filter with new one
    pub fn add_filter<U>(mut self, nf: U) -> Io<Layer<U, F>>
    where
        U: FilterLayer,
    {
        // add layer to buffers
        if U::BUFFERS {
            // Safety: .add_layer() only increases internal buffers
            // there is no api that holds references into buffers storage
            // all apis first removes buffer from storage and then work with it
            unsafe { &mut *(Rc::as_ptr(&self.0 .0) as *mut IoState) }
                .buffer
                .add_layer();
        }

        // replace current filter
        let (filter, filter_ref) = self.1.add_filter(nf);
        self.0 .0.filter.replace(filter_ref);
        Io(self.0.clone(), filter)
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
                Err(RecvError::Stop) => Err(Either::Right(io::Error::new(
                    io::ErrorKind::Other,
                    "Dispatcher stopped",
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

    #[doc(hidden)]
    #[inline]
    /// Wait until read becomes ready.
    pub async fn force_read_ready(&self) -> io::Result<Option<()>> {
        poll_fn(|cx| self.poll_force_read_ready(cx)).await
    }

    #[inline]
    /// Pause read task
    pub fn pause(&self) {
        if !self.0.flags().contains(Flags::RD_PAUSED) {
            self.0 .0.read_task.wake();
            self.0 .0.insert_flags(Flags::RD_PAUSED);
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
    /// This is async version of .poll_flush() method.
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
    /// When the io stream becomes ready for reading, Waker::wake will be called on the waker.
    ///
    /// Return value
    /// The function returns:
    ///
    /// `Poll::Pending` if the io stream is not ready for reading.
    /// `Poll::Ready(Ok(Some(()))))` if the io stream is ready for reading.
    /// `Poll::Ready(Ok(None))` if io stream is disconnected
    /// `Some(Poll::Ready(Err(e)))` if an error is encountered.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<Option<()>>> {
        let mut flags = self.0 .0.flags.get();

        if flags.contains(Flags::IO_STOPPED) {
            Poll::Ready(self.error().map(Err).unwrap_or(Ok(None)))
        } else {
            self.0 .0.dispatch_task.register(cx.waker());

            let ready = flags.contains(Flags::RD_READY);
            if flags.intersects(Flags::RD_BUF_FULL | Flags::RD_PAUSED) {
                flags.remove(Flags::RD_READY | Flags::RD_BUF_FULL | Flags::RD_PAUSED);
                self.0 .0.read_task.wake();
                self.0 .0.flags.set(flags);
                if ready {
                    Poll::Ready(Ok(Some(())))
                } else {
                    Poll::Pending
                }
            } else if ready {
                flags.remove(Flags::RD_READY);
                self.0 .0.flags.set(flags);
                Poll::Ready(Ok(Some(())))
            } else {
                Poll::Pending
            }
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Polls for read readiness.
    ///
    /// If the io stream is not currently ready for reading,
    /// this method will store a clone of the Waker from the provided Context.
    /// When the io stream becomes ready for reading, Waker::wake will be called on the waker.
    ///
    /// Return value
    /// The function returns:
    ///
    /// `Poll::Pending` if the io stream is not ready for reading.
    /// `Poll::Ready(Ok(Some(()))))` if the io stream is ready for reading.
    /// `Poll::Ready(Ok(None))` if io stream is disconnected
    /// `Some(Poll::Ready(Err(e)))` if an error is encountered.
    pub fn poll_force_read_ready(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<Option<()>>> {
        let ready = self.poll_read_ready(cx);

        if ready.is_pending() {
            if self.0 .0.remove_flags(Flags::RD_FORCE_READY) {
                Poll::Ready(Ok(Some(())))
            } else {
                self.0 .0.insert_flags(Flags::RD_FORCE_READY);
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
            let flags = self.flags();
            if flags.contains(Flags::IO_STOPPED) {
                Err(RecvError::PeerGone(self.error()))
            } else if flags.contains(Flags::DSP_STOP) {
                self.0 .0.remove_flags(Flags::DSP_STOP);
                Err(RecvError::Stop)
            } else if flags.contains(Flags::DSP_TIMEOUT) {
                self.0 .0.remove_flags(Flags::DSP_TIMEOUT);
                Err(RecvError::KeepAlive)
            } else if flags.contains(Flags::WR_BACKPRESSURE) {
                Err(RecvError::WriteBackpressure)
            } else {
                match self.poll_read_ready(cx) {
                    Poll::Pending | Poll::Ready(Ok(Some(()))) => {
                        if log::log_enabled!(log::Level::Debug) && decoded.remains != 0 {
                            log::debug!(
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
        let flags = self.flags();

        if flags.contains(Flags::IO_STOPPED) {
            Poll::Ready(self.error().map(Err).unwrap_or(Ok(())))
        } else {
            let inner = &self.0 .0;
            let len = inner.buffer.write_destination_size();
            if len > 0 {
                if full {
                    inner.insert_flags(Flags::WR_WAIT);
                    inner.dispatch_task.register(cx.waker());
                    return Poll::Pending;
                } else if len >= inner.pool.get().write_params_high() << 1 {
                    inner.insert_flags(Flags::WR_BACKPRESSURE);
                    inner.dispatch_task.register(cx.waker());
                    return Poll::Pending;
                }
            }
            inner.remove_flags(Flags::WR_WAIT | Flags::WR_BACKPRESSURE);
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    /// Gracefully shutdown io stream
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let flags = self.flags();

        if flags.intersects(Flags::IO_STOPPED) {
            if let Some(err) = self.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(()))
            }
        } else {
            if !flags.contains(Flags::IO_STOPPING_FILTERS) {
                self.0 .0.init_shutdown();
            }

            self.0 .0.read_task.wake();
            self.0 .0.write_task.wake();
            self.0 .0.dispatch_task.register(cx.waker());
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
            self.0 .0.dispatch_task.register(cx.waker());
        }
        result
    }

    #[inline]
    /// Wait for status updates
    pub fn poll_status_update(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        let flags = self.flags();
        if flags.intersects(Flags::IO_STOPPED | Flags::IO_STOPPING) {
            Poll::Ready(IoStatusUpdate::PeerGone(self.error()))
        } else if flags.contains(Flags::DSP_STOP) {
            self.0 .0.remove_flags(Flags::DSP_STOP);
            Poll::Ready(IoStatusUpdate::Stop)
        } else if flags.contains(Flags::DSP_TIMEOUT) {
            self.0 .0.remove_flags(Flags::DSP_TIMEOUT);
            Poll::Ready(IoStatusUpdate::KeepAlive)
        } else if flags.contains(Flags::WR_BACKPRESSURE) {
            Poll::Ready(IoStatusUpdate::WriteBackpressure)
        } else {
            self.0 .0.dispatch_task.register(cx.waker());
            Poll::Pending
        }
    }

    #[inline]
    /// Register dispatch task
    pub fn poll_dispatch(&self, cx: &mut Context<'_>) {
        self.0 .0.dispatch_task.register(cx.waker());
    }
}

impl<F> AsRef<IoRef> for Io<F> {
    #[inline]
    fn as_ref(&self) -> &IoRef {
        &self.0
    }
}

impl<F> Eq for Io<F> {}

impl<F> PartialEq for Io<F> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

impl<F> hash::Hash for Io<F> {
    #[inline]
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<F> fmt::Debug for Io<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Io").field("state", &self.0).finish()
    }
}

impl<F> ops::Deref for Io<F> {
    type Target = IoRef;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<F> Drop for Io<F> {
    fn drop(&mut self) {
        self.stop_timer();

        if !self.0.flags().contains(Flags::IO_STOPPED) {
            log::trace!(
                "{}: Io is dropped, force stopping io streams {:?}",
                self.tag(),
                self.0.flags()
            );

            self.force_close();
        }

        // filter must be dropped, it is unsafe
        // and wont be dropped without special attention
        if self.1.is_set() {
            self.1.drop_filter();
            self.0 .0.filter.set(NullFilter::get());
        }
    }
}

const KIND_SEALED: u8 = 0b01;
const KIND_PTR: u8 = 0b10;
const KIND_MASK: u8 = 0b11;
const KIND_UNMASK: u8 = !KIND_MASK;
const KIND_MASK_USIZE: usize = 0b11;
const KIND_UNMASK_USIZE: usize = !KIND_MASK_USIZE;
const SEALED_SIZE: usize = mem::size_of::<Sealed>();

#[cfg(target_endian = "little")]
const KIND_IDX: usize = 0;

#[cfg(target_endian = "big")]
const KIND_IDX: usize = SEALED_SIZE - 1;

struct FilterItem<F> {
    data: [u8; SEALED_SIZE],
    _t: marker::PhantomData<F>,
}

impl<F> FilterItem<F> {
    fn null() -> Self {
        Self {
            data: [0; SEALED_SIZE],
            _t: marker::PhantomData,
        }
    }

    fn with_filter(f: Box<F>) -> Self {
        let mut slf = Self {
            data: [0; SEALED_SIZE],
            _t: marker::PhantomData,
        };

        unsafe {
            let ptr = &mut slf.data as *mut _ as *mut *mut F;
            ptr.write(Box::into_raw(f));
            slf.data[KIND_IDX] |= KIND_PTR;
        }
        slf
    }

    /// Get filter, panic if it is not filter
    fn filter(&self) -> &F {
        if self.data[KIND_IDX] & KIND_PTR != 0 {
            let ptr = &self.data as *const _ as *const *mut F;
            unsafe {
                let p = (ptr.read() as *const _ as usize) & KIND_UNMASK_USIZE;
                (p as *const F as *mut F).as_ref().unwrap()
            }
        } else {
            panic!("Wrong filter item");
        }
    }

    /// Get filter, panic if it is not set
    fn take_filter(&mut self) -> Box<F> {
        if self.data[KIND_IDX] & KIND_PTR != 0 {
            self.data[KIND_IDX] &= KIND_UNMASK;
            let ptr = &mut self.data as *mut _ as *mut *mut F;
            unsafe { Box::from_raw(*ptr) }
        } else {
            panic!(
                "Wrong filter item {:?} expected: {:?}",
                self.data[KIND_IDX], KIND_PTR
            );
        }
    }

    /// Get sealed, panic if it is already sealed
    fn take_sealed(&mut self) -> Sealed {
        if self.data[KIND_IDX] & KIND_SEALED != 0 {
            self.data[KIND_IDX] &= KIND_UNMASK;
            let ptr = &mut self.data as *mut _ as *mut Sealed;
            unsafe { ptr.read() }
        } else {
            panic!(
                "Wrong filter item {:?} expected: {:?}",
                self.data[KIND_IDX], KIND_SEALED
            );
        }
    }

    fn is_set(&self) -> bool {
        self.data[KIND_IDX] & KIND_MASK != 0
    }

    fn drop_filter(&mut self) {
        if self.data[KIND_IDX] & KIND_PTR != 0 {
            self.take_filter();
        } else if self.data[KIND_IDX] & KIND_SEALED != 0 {
            self.take_sealed();
        }
    }
}

impl<F: Filter> FilterItem<F> {
    fn add_filter<T: FilterLayer>(
        &mut self,
        new: T,
    ) -> (FilterItem<Layer<T, F>>, &'static dyn Filter) {
        let filter = Box::new(Layer::new(new, *self.take_filter()));
        let filter_ref: &'static dyn Filter = {
            let filter: &dyn Filter = filter.as_ref();
            unsafe { std::mem::transmute(filter) }
        };
        (FilterItem::with_filter(filter), filter_ref)
    }

    fn seal(&mut self) -> (FilterItem<Sealed>, &'static dyn Filter) {
        let filter = if self.data[KIND_IDX] & KIND_PTR != 0 {
            Sealed(Box::new(*self.take_filter()))
        } else if self.data[KIND_IDX] & KIND_SEALED != 0 {
            self.take_sealed()
        } else {
            panic!(
                "Wrong filter item {:?} expected: {:?}",
                self.data[KIND_IDX], KIND_PTR
            );
        };

        let filter_ref: &'static dyn Filter = {
            let filter: &dyn Filter = filter.0.as_ref();
            unsafe { std::mem::transmute(filter) }
        };

        let mut slf = FilterItem {
            data: [0; SEALED_SIZE],
            _t: marker::PhantomData,
        };

        unsafe {
            let ptr = &mut slf.data as *mut _ as *mut Sealed;
            ptr.write(filter);
            slf.data[KIND_IDX] |= KIND_SEALED;
        }
        (slf, filter_ref)
    }
}

#[derive(Debug)]
/// OnDisconnect future resolves when socket get disconnected
#[must_use = "OnDisconnect do nothing unless polled"]
pub struct OnDisconnect {
    token: usize,
    inner: Rc<IoState>,
}

impl OnDisconnect {
    pub(super) fn new(inner: Rc<IoState>) -> Self {
        Self::new_inner(inner.flags.get().contains(Flags::IO_STOPPED), inner)
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
        if self.token == usize::MAX || self.inner.flags.get().contains(Flags::IO_STOPPED) {
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

    #[ntex::test]
    async fn test_basics() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::new(server);
        assert!(server.eq(&server));
        assert!(server.0.eq(&server.0));

        assert!(format!("{:?}", Flags::IO_STOPPED).contains("IO_STOPPED"));
        assert!(Flags::IO_STOPPED == Flags::IO_STOPPED);
        assert!(Flags::IO_STOPPED != Flags::IO_STOPPING);
    }

    #[ntex::test]
    async fn test_recv() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::new(server);

        server.0 .0.notify_timeout();
        let err = server.recv(&BytesCodec).await.err().unwrap();
        assert!(format!("{:?}", err).contains("Timeout"));

        server.0 .0.insert_flags(Flags::DSP_STOP);
        let err = server.recv(&BytesCodec).await.err().unwrap();
        assert!(format!("{:?}", err).contains("Dispatcher stopped"));

        client.write("GET /test HTTP/1");
        server.0 .0.insert_flags(Flags::WR_BACKPRESSURE);
        let item = server.recv(&BytesCodec).await.ok().unwrap().unwrap();
        assert_eq!(item, "GET /test HTTP/1");
    }

    #[ntex::test]
    async fn test_send() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::new(server);
        assert!(server.eq(&server));

        server
            .send(Bytes::from_static(b"GET /test HTTP/1"), &BytesCodec)
            .await
            .ok()
            .unwrap();
        let item = client.read_any();
        assert_eq!(item, "GET /test HTTP/1");
    }
}
