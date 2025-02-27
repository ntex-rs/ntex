use std::cell::{Cell, UnsafeCell};
use std::future::{poll_fn, Future};
use std::task::{Context, Poll};
use std::{fmt, hash, io, marker, mem, ops, pin::Pin, ptr, rc::Rc};

use ntex_bytes::{PoolId, PoolRef};
use ntex_codec::{Decoder, Encoder};
use ntex_util::{future::Either, task::LocalWaker, time::Seconds};

use crate::buf::Stack;
use crate::filter::{Base, Filter, Layer, NullFilter};
use crate::flags::Flags;
use crate::seal::{IoBoxed, Sealed};
use crate::tasks::{ReadContext, WriteContext};
use crate::timer::TimerHandle;
use crate::{Decoded, FilterLayer, Handle, IoStatusUpdate, IoStream, RecvError};

/// Interface object to underlying io stream
pub struct Io<F = Base>(UnsafeCell<IoRef>, marker::PhantomData<F>);

#[derive(Clone)]
pub struct IoRef(pub(super) Rc<IoState>);

pub(crate) struct IoState {
    filter: FilterPtr,
    pub(super) flags: Cell<Flags>,
    pub(super) pool: Cell<PoolRef>,
    pub(super) disconnect_timeout: Cell<Seconds>,
    pub(super) error: Cell<Option<io::Error>>,
    pub(super) read_task: LocalWaker,
    pub(super) write_task: LocalWaker,
    pub(super) dispatch_task: LocalWaker,
    pub(super) buffer: Stack,
    pub(super) handle: Cell<Option<Box<dyn Handle>>>,
    pub(super) timeout: Cell<TimerHandle>,
    pub(super) tag: Cell<&'static str>,
    #[allow(clippy::box_collection)]
    pub(super) on_disconnect: Cell<Option<Box<Vec<LocalWaker>>>>,
}

const DEFAULT_TAG: &str = "IO";

impl IoState {
    pub(super) fn filter(&self) -> &dyn Filter {
        self.filter.filter.get()
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
            log::trace!("{}: Timer, notify dispatcher", self.tag.get());
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
                .set(Some(io::Error::new(err.kind(), format!("{}", err))));
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
            .field("filter", &self.filter.is_set())
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
            filter: FilterPtr::null(),
            pool: Cell::new(pool),
            flags: Cell::new(Flags::WR_PAUSED),
            error: Cell::new(None),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            buffer: Stack::new(),
            handle: Cell::new(None),
            timeout: Cell::new(TimerHandle::default()),
            disconnect_timeout: Cell::new(Seconds(1)),
            on_disconnect: Cell::new(None),
            tag: Cell::new(DEFAULT_TAG),
        });
        inner.filter.update(Base::new(IoRef(inner.clone())));

        let io_ref = IoRef(inner);

        // start io tasks
        let hnd = io.start(ReadContext::new(&io_ref), WriteContext::new(&io_ref));
        io_ref.0.handle.set(hnd);

        Io(UnsafeCell::new(io_ref), marker::PhantomData)
    }
}

impl<F> Io<F> {
    #[inline]
    /// Set memory pool
    pub fn set_memory_pool(&self, pool: PoolRef) {
        self.st().buffer.set_memory_pool(pool);
        self.st().pool.set(pool);
    }

    #[inline]
    /// Set io disconnect timeout in millis
    pub fn set_disconnect_timeout(&self, timeout: Seconds) {
        self.st().disconnect_timeout.set(timeout);
    }

    #[inline]
    /// Clone current io object.
    ///
    /// Current io object becomes closed.
    pub fn take(&self) -> Self {
        Self(UnsafeCell::new(self.take_io_ref()), marker::PhantomData)
    }

    fn take_io_ref(&self) -> IoRef {
        let inner = Rc::new(IoState {
            filter: FilterPtr::null(),
            pool: self.st().pool.clone(),
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
            handle: Cell::new(None),
            timeout: Cell::new(TimerHandle::default()),
            on_disconnect: Cell::new(None),
            tag: Cell::new(DEFAULT_TAG),
        });
        unsafe { mem::replace(&mut *self.0.get(), IoRef(inner)) }
    }
}

impl<F> Io<F> {
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
    /// Map current filter with new one
    pub fn add_filter<U>(self, nf: U) -> Io<Layer<U, F>>
    where
        U: FilterLayer,
    {
        let state = self.take_io_ref();

        // add layer to buffers
        if U::BUFFERS {
            // Safety: .add_layer() only increases internal buffers
            // there is no api that holds references into buffers storage
            // all apis first removes buffer from storage and then work with it
            unsafe { &mut *(Rc::as_ptr(&state.0) as *mut IoState) }
                .buffer
                .add_layer();
        }

        // replace current filter
        state.0.filter.add_filter::<F, U>(nf);

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
                Err(RecvError::Stop) => Err(Either::Right(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
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
        let st = self.st();
        let mut flags = st.flags.get();

        if flags.is_stopped() {
            Poll::Ready(Err(st.error_or_disconnected()))
        } else {
            st.dispatch_task.register(cx.waker());

            let ready = flags.contains(Flags::BUF_R_READY);
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
            } else if flags.contains(Flags::DSP_STOP) {
                st.remove_flags(Flags::DSP_STOP);
                Err(RecvError::Stop)
            } else if flags.contains(Flags::DSP_TIMEOUT) {
                st.remove_flags(Flags::DSP_TIMEOUT);
                Err(RecvError::KeepAlive)
            } else if flags.contains(Flags::BUF_W_BACKPRESSURE) {
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
        let st = self.st();
        let flags = self.flags();

        if flags.is_stopped() {
            Poll::Ready(Err(st.error_or_disconnected()))
        } else {
            let len = st.buffer.write_destination_size();
            if len > 0 {
                if full {
                    st.insert_flags(Flags::BUF_W_MUST_FLUSH);
                    st.dispatch_task.register(cx.waker());
                    return Poll::Pending;
                } else if len >= st.pool.get().write_params_high() << 1 {
                    st.insert_flags(Flags::BUF_W_BACKPRESSURE);
                    st.dispatch_task.register(cx.waker());
                    return Poll::Pending;
                }
            }
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
        } else if flags.contains(Flags::DSP_STOP) {
            st.remove_flags(Flags::DSP_STOP);
            Poll::Ready(IoStatusUpdate::Stop)
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
            // and wont be dropped without special attention
            if !st.flags.get().is_stopped() {
                log::trace!(
                    "{}: Io is dropped, force stopping io streams {:?}",
                    st.tag.get(),
                    st.flags.get()
                );
            }

            self.force_close();
            st.filter.drop_filter::<F>();
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
const NULL: [u8; SEALED_SIZE] = [0u8; SEALED_SIZE];

#[cfg(target_endian = "little")]
const KIND_IDX: usize = 0;

#[cfg(target_endian = "big")]
const KIND_IDX: usize = SEALED_SIZE - 1;

struct FilterPtr {
    data: Cell<[u8; SEALED_SIZE]>,
    filter: Cell<&'static dyn Filter>,
}

impl FilterPtr {
    const fn null() -> Self {
        Self {
            data: Cell::new(NULL),
            filter: Cell::new(NullFilter::get()),
        }
    }

    fn update<F: Filter>(&self, filter: F) {
        if self.is_set() {
            panic!("Filter is set, must be dropped first");
        }

        let filter = Box::new(filter);
        let mut data = NULL;
        unsafe {
            let filter_ref: &'static dyn Filter = {
                let f: &dyn Filter = filter.as_ref();
                std::mem::transmute(f)
            };
            self.filter.set(filter_ref);

            let ptr = &mut data as *mut _ as *mut *mut F;
            ptr.write(Box::into_raw(filter));
            data[KIND_IDX] |= KIND_PTR;
            self.data.set(data);
        }
    }

    /// Get filter, panic if it is not filter
    fn filter<F: Filter>(&self) -> &F {
        let data = self.data.get();
        if data[KIND_IDX] & KIND_PTR != 0 {
            let ptr = &data as *const _ as *const *mut F;
            unsafe {
                let p = (ptr.read() as *const _ as usize) & KIND_UNMASK_USIZE;
                (p as *const F as *mut F).as_ref().unwrap()
            }
        } else {
            panic!("Wrong filter item");
        }
    }

    /// Get filter, panic if it is not set
    fn take_filter<F>(&self) -> Box<F> {
        let mut data = self.data.get();
        if data[KIND_IDX] & KIND_PTR != 0 {
            data[KIND_IDX] &= KIND_UNMASK;
            let ptr = &mut data as *mut _ as *mut *mut F;
            unsafe { Box::from_raw(*ptr) }
        } else {
            panic!(
                "Wrong filter item {:?} expected: {:?}",
                data[KIND_IDX], KIND_PTR
            );
        }
    }

    /// Get sealed, panic if it is already sealed
    fn take_sealed(&self) -> Sealed {
        let mut data = self.data.get();

        if data[KIND_IDX] & KIND_SEALED != 0 {
            data[KIND_IDX] &= KIND_UNMASK;
            let ptr = &mut data as *mut _ as *mut Sealed;
            unsafe { ptr.read() }
        } else {
            panic!(
                "Wrong filter item {:?} expected: {:?}",
                data[KIND_IDX], KIND_SEALED
            );
        }
    }

    fn is_set(&self) -> bool {
        self.data.get()[KIND_IDX] & KIND_MASK != 0
    }

    fn drop_filter<F>(&self) {
        let data = self.data.get();

        if data[KIND_IDX] & KIND_MASK != 0 {
            if data[KIND_IDX] & KIND_PTR != 0 {
                self.take_filter::<F>();
            } else if data[KIND_IDX] & KIND_SEALED != 0 {
                self.take_sealed();
            }
            self.data.set(NULL);
            self.filter.set(NullFilter::get());
        }
    }
}

impl FilterPtr {
    fn add_filter<F: Filter, T: FilterLayer>(&self, new: T) {
        let mut data = NULL;
        let filter = Box::new(Layer::new(new, *self.take_filter::<F>()));
        unsafe {
            let filter_ref: &'static dyn Filter = {
                let f: &dyn Filter = filter.as_ref();
                std::mem::transmute(f)
            };
            self.filter.set(filter_ref);

            let ptr = &mut data as *mut _ as *mut *mut Layer<T, F>;
            ptr.write(Box::into_raw(filter));
            data[KIND_IDX] |= KIND_PTR;
            self.data.set(data);
        }
    }

    fn seal<F: Filter>(&self) {
        let mut data = self.data.get();

        let filter = if data[KIND_IDX] & KIND_PTR != 0 {
            Sealed(Box::new(*self.take_filter::<F>()))
        } else if data[KIND_IDX] & KIND_SEALED != 0 {
            self.take_sealed()
        } else {
            panic!(
                "Wrong filter item {:?} expected: {:?}",
                data[KIND_IDX], KIND_PTR
            );
        };

        unsafe {
            let filter_ref: &'static dyn Filter = {
                let f: &dyn Filter = filter.0.as_ref();
                std::mem::transmute(f)
            };
            self.filter.set(filter_ref);

            let ptr = &mut data as *mut _ as *mut Sealed;
            ptr.write(filter);
            data[KIND_IDX] |= KIND_SEALED;
            self.data.set(data);
        }
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
    use crate::{testing::IoTest, ReadBuf, WriteBuf};

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[ntex::test]
    async fn test_basics() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::new(server);
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

        let server = Io::new(server);

        server.st().notify_timeout();
        let err = server.recv(&BytesCodec).await.err().unwrap();
        assert!(format!("{:?}", err).contains("Timeout"));

        server.st().insert_flags(Flags::DSP_STOP);
        let err = server.recv(&BytesCodec).await.err().unwrap();
        assert!(format!("{:?}", err).contains("Dispatcher stopped"));

        client.write(TEXT);
        server.st().insert_flags(Flags::BUF_W_BACKPRESSURE);
        let item = server.recv(&BytesCodec).await.ok().unwrap().unwrap();
        assert_eq!(item, TEXT);
    }

    #[ntex::test]
    async fn test_send() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let server = Io::new(server);
        assert!(server.eq(&server));

        server
            .send(Bytes::from_static(BIN), &BytesCodec)
            .await
            .ok()
            .unwrap();
        let item = client.read_any();
        assert_eq!(item, TEXT);
    }

    #[derive(Debug)]
    struct DropFilter {
        p: Rc<Cell<usize>>,
    }

    impl Drop for DropFilter {
        fn drop(&mut self) {
            self.p.set(self.p.get() + 1);
        }
    }

    impl FilterLayer for DropFilter {
        const BUFFERS: bool = false;
        fn process_read_buf(&self, buf: &ReadBuf<'_>) -> io::Result<usize> {
            Ok(buf.nbytes())
        }
        fn process_write_buf(&self, _: &WriteBuf<'_>) -> io::Result<()> {
            Ok(())
        }
    }

    #[ntex::test]
    async fn drop_filter() {
        let p = Rc::new(Cell::new(0));

        let (client, server) = IoTest::create();
        let f = DropFilter { p: p.clone() };
        let _ = format!("{:?}", f);
        let io = Io::new(server).add_filter(f);

        client.remote_buffer_cap(1024);
        client.write(TEXT);
        let msg = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        io.send(Bytes::from_static(b"test"), &BytesCodec)
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        let io2 = io.take();
        let mut io3: crate::IoBoxed = io2.into();
        let io4 = io3.take();

        drop(io);
        drop(io3);
        drop(io4);

        assert_eq!(p.get(), 1);
    }
}
