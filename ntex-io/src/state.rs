use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{any, fmt, future::Future, hash, io, mem, ops::Deref, pin::Pin, ptr, rc::Rc};

use ntex_bytes::{BytesMut, PoolId, PoolRef};
use ntex_codec::{Decoder, Encoder};
use ntex_util::time::Millis;
use ntex_util::{future::poll_fn, future::Either, task::LocalWaker};

use super::filter::{DefaultFilter, NullFilter};
use super::tasks::{ReadContext, WriteContext};
use super::{types, Filter, FilterFactory, Handle, IoStream};

bitflags::bitflags! {
    pub struct Flags: u16 {
        /// io error occured
        const IO_ERR          = 0b0000_0000_0000_0001;
        /// shuting down filters
        const IO_FILTERS      = 0b0000_0000_0000_0010;
        /// shuting down filters timeout
        const IO_FILTERS_TO   = 0b0000_0000_0000_0100;
        /// shutdown io tasks
        const IO_SHUTDOWN     = 0b0000_0000_0000_1000;
        /// io object is closed
        const IO_CLOSED       = 0b0000_0000_0001_0000;

        /// pause io read
        const RD_PAUSED       = 0b0000_0000_0010_0000;
        /// new data is available
        const RD_READY        = 0b0000_0000_0100_0000;
        /// read buffer is full
        const RD_BUF_FULL     = 0b0000_0000_1000_0000;

        /// wait write completion
        const WR_WAIT         = 0b0000_0001_0000_0000;
        /// write buffer is full
        const WR_BACKPRESSURE = 0b0000_0010_0000_0000;

        /// dispatcher is marked stopped
        const DSP_STOP        = 0b0001_0000_0000_0000;
        /// keep-alive timeout occured
        const DSP_KEEPALIVE   = 0b0010_0000_0000_0000;
        /// dispatcher returned error
        const DSP_ERR         = 0b0100_0000_0000_0000;
    }
}

enum FilterItem<F> {
    Boxed(Box<dyn Filter>),
    Ptr(*mut F),
}

pub struct Io<F = DefaultFilter>(pub(super) IoRef, FilterItem<F>);

#[derive(Clone)]
pub struct IoRef(pub(super) Rc<IoStateInner>);

pub(crate) struct IoStateInner {
    pub(super) flags: Cell<Flags>,
    pub(super) pool: Cell<PoolRef>,
    pub(super) disconnect_timeout: Cell<Millis>,
    pub(super) error: Cell<Option<io::Error>>,
    pub(super) read_task: LocalWaker,
    pub(super) write_task: LocalWaker,
    pub(super) dispatch_task: LocalWaker,
    pub(super) read_buf: Cell<Option<BytesMut>>,
    pub(super) write_buf: Cell<Option<BytesMut>>,
    filter: Cell<&'static dyn Filter>,
    pub(super) handle: Cell<Option<Box<dyn Handle>>>,
    on_disconnect: RefCell<Vec<Option<LocalWaker>>>,
}

impl IoStateInner {
    #[inline]
    pub(super) fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    #[inline]
    pub(super) fn remove_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.remove(f);
        self.flags.set(flags);
    }

    #[inline]
    pub(super) fn notify_keepalive(&self) {
        let mut flags = self.flags.get();
        if !flags.contains(Flags::DSP_KEEPALIVE) {
            flags.insert(Flags::DSP_KEEPALIVE);
            self.flags.set(flags);
            self.dispatch_task.wake();
        }
    }

    #[inline]
    pub(super) fn notify_disconnect(&self) {
        let mut on_disconnect = self.on_disconnect.borrow_mut();
        for item in &mut *on_disconnect {
            if let Some(waker) = item.take() {
                waker.wake();
            }
        }
    }

    #[inline]
    fn is_io_open(&self) -> bool {
        !self.flags.get().intersects(
            Flags::IO_ERR | Flags::IO_SHUTDOWN | Flags::IO_SHUTDOWN | Flags::IO_CLOSED,
        )
    }

    #[inline]
    pub(super) fn set_error(&self, err: Option<io::Error>) {
        if err.is_some() {
            self.error.set(err);
        }
        self.read_task.wake();
        self.write_task.wake();
        self.dispatch_task.wake();
        self.insert_flags(Flags::IO_ERR | Flags::DSP_STOP);
        self.notify_disconnect();
    }

    #[inline]
    /// Gracefully shutdown read and write io tasks
    pub(super) fn init_shutdown(&self, cx: Option<&mut Context<'_>>, st: &IoRef) {
        let mut flags = self.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN | Flags::IO_FILTERS) {
            log::trace!("initiate io shutdown {:?}", flags);
            flags.insert(Flags::IO_FILTERS);
            if let Err(err) = self.shutdown_filters(st) {
                self.error.set(Some(err));
                flags.insert(Flags::IO_ERR);
            }

            self.flags.set(flags);
            self.read_task.wake();
            self.write_task.wake();
            if let Some(cx) = cx {
                self.dispatch_task.register(cx.waker());
            }
        }
    }

    #[inline]
    pub(super) fn shutdown_filters(&self, st: &IoRef) -> Result<(), io::Error> {
        let mut flags = self.flags.get();
        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            let result = match self.filter.get().shutdown(st) {
                Poll::Pending => return Ok(()),
                Poll::Ready(Ok(())) => {
                    flags.insert(Flags::IO_SHUTDOWN);
                    Ok(())
                }
                Poll::Ready(Err(err)) => {
                    flags.insert(Flags::IO_ERR);
                    self.dispatch_task.wake();
                    Err(err)
                }
            };
            self.flags.set(flags);
            self.read_task.wake();
            self.write_task.wake();
            result
        } else {
            Ok(())
        }
    }
}

impl Eq for IoStateInner {}

impl PartialEq for IoStateInner {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self, other)
    }
}

impl hash::Hash for IoStateInner {
    #[inline]
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        (self as *const _ as usize).hash(state);
    }
}

impl Drop for IoStateInner {
    #[inline]
    fn drop(&mut self) {
        if let Some(buf) = self.read_buf.take() {
            self.pool.get().release_read_buf(buf);
        }
        if let Some(buf) = self.write_buf.take() {
            self.pool.get().release_write_buf(buf);
        }
    }
}

impl Io {
    #[inline]
    /// Create `State` instance
    pub fn new<I: IoStream>(io: I) -> Self {
        Self::with_memory_pool(io, PoolId::DEFAULT.pool_ref())
    }

    #[inline]
    /// Create `State` instance with specific memory pool.
    pub fn with_memory_pool<I: IoStream>(io: I, pool: PoolRef) -> Self {
        let inner = Rc::new(IoStateInner {
            pool: Cell::new(pool),
            flags: Cell::new(Flags::empty()),
            error: Cell::new(None),
            disconnect_timeout: Cell::new(Millis::ONE_SEC),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            read_buf: Cell::new(None),
            write_buf: Cell::new(None),
            filter: Cell::new(NullFilter::get()),
            handle: Cell::new(None),
            on_disconnect: RefCell::new(Vec::new()),
        });

        let filter = Box::new(DefaultFilter::new(IoRef(inner.clone())));
        let filter_ref: &'static dyn Filter = unsafe {
            let filter: &dyn Filter = filter.as_ref();
            std::mem::transmute(filter)
        };
        inner.filter.replace(filter_ref);

        let io_ref = IoRef(inner);

        // start io tasks
        let hnd = io.start(ReadContext(io_ref.clone()), WriteContext(io_ref.clone()));
        io_ref.0.handle.set(hnd);

        Io(io_ref, FilterItem::Ptr(Box::into_raw(filter)))
    }
}

impl<F> Io<F> {
    #[inline]
    /// Set memory pool
    pub fn set_memory_pool(&self, pool: PoolRef) {
        if let Some(mut buf) = self.0 .0.read_buf.take() {
            pool.move_in(&mut buf);
            self.0 .0.read_buf.set(Some(buf));
        }
        if let Some(mut buf) = self.0 .0.write_buf.take() {
            pool.move_in(&mut buf);
            self.0 .0.write_buf.set(Some(buf));
        }
        self.0 .0.pool.set(pool);
    }

    #[inline]
    /// Set io disconnect timeout in secs
    pub fn set_disconnect_timeout(&self, timeout: Millis) {
        self.0 .0.disconnect_timeout.set(timeout);
    }
}

impl<F> Io<F> {
    #[inline]
    #[allow(clippy::should_implement_trait)]
    /// Get IoRef reference
    pub fn as_ref(&self) -> &IoRef {
        &self.0
    }

    #[inline]
    /// Get instance of IoRef
    pub fn get_ref(&self) -> IoRef {
        self.0.clone()
    }

    #[inline]
    /// Register dispatcher task
    pub fn register_dispatcher(&self, cx: &mut Context<'_>) {
        self.0 .0.dispatch_task.register(cx.waker());
    }
}

impl IoRef {
    #[inline]
    #[doc(hidden)]
    /// Get current state flags
    pub fn flags(&self) -> Flags {
        self.0.flags.get()
    }

    #[inline]
    /// Set flags
    pub(crate) fn set_flags(&self, flags: Flags) {
        self.0.flags.set(flags)
    }

    #[inline]
    /// Get memory pool
    pub(crate) fn filter(&self) -> &dyn Filter {
        self.0.filter.get()
    }

    #[inline]
    /// Get memory pool
    pub fn memory_pool(&self) -> PoolRef {
        self.0.pool.get()
    }

    #[inline]
    /// Check if io is still active
    pub fn is_io_open(&self) -> bool {
        self.0.is_io_open()
    }

    #[inline]
    /// Check if keep-alive timeout occured
    pub fn is_keepalive(&self) -> bool {
        self.0.flags.get().contains(Flags::DSP_KEEPALIVE)
    }

    #[inline]
    /// Check if dispatcher marked stopped
    pub fn is_dispatcher_stopped(&self) -> bool {
        self.0.flags.get().contains(Flags::DSP_STOP)
    }

    #[inline]
    /// Check if io stream is closed
    pub fn is_closed(&self) -> bool {
        self.0.flags.get().intersects(
            Flags::IO_ERR
                | Flags::IO_SHUTDOWN
                | Flags::IO_CLOSED
                | Flags::IO_FILTERS
                | Flags::DSP_STOP,
        )
    }

    #[inline]
    /// Take io error if any occured
    pub fn take_error(&self) -> Option<io::Error> {
        self.0.error.take()
    }

    #[inline]
    /// Mark dispatcher as stopped
    pub fn stop_dispatcher(&self) {
        self.0.insert_flags(Flags::DSP_STOP);
    }

    #[inline]
    /// Reset keep-alive error
    pub fn reset_keepalive(&self) {
        self.0.remove_flags(Flags::DSP_KEEPALIVE)
    }

    #[inline]
    /// Get api for read task
    pub fn read(&'_ self) -> ReadRef<'_> {
        ReadRef(self)
    }

    #[inline]
    /// Get api for write task
    pub fn write(&'_ self) -> WriteRef<'_> {
        WriteRef(self)
    }

    #[inline]
    /// Gracefully close connection
    ///
    /// First stop dispatcher, then dispatcher stops io tasks
    pub fn close(&self) {
        self.0.insert_flags(Flags::DSP_STOP);
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Force close connection
    ///
    /// Dispatcher does not wait for uncompleted responses, but flushes io buffers.
    pub fn force_close(&self) {
        log::trace!("force close framed object");
        self.0.insert_flags(Flags::DSP_STOP | Flags::IO_SHUTDOWN);
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Notify when io stream get disconnected
    pub fn on_disconnect(&self) -> OnDisconnect {
        OnDisconnect::new(self.0.clone(), self.0.flags.get().contains(Flags::IO_ERR))
    }

    #[inline]
    /// Query specific data
    pub fn query<T: 'static>(&self) -> types::QueryItem<T> {
        if let Some(item) = self.filter().query(any::TypeId::of::<T>()) {
            types::QueryItem::new(item)
        } else {
            types::QueryItem::empty()
        }
    }
}

impl IoRef {
    #[inline]
    /// Read incoming io stream and decode codec item.
    pub async fn next<U>(
        &self,
        codec: &U,
    ) -> Result<Option<U::Item>, Either<U::Error, io::Error>>
    where
        U: Decoder,
    {
        poll_fn(|cx| self.poll_next(codec, cx)).await
    }

    #[inline]
    /// Encode item, send to a peer
    pub async fn send<U>(
        &self,
        item: U::Item,
        codec: &U,
    ) -> Result<(), Either<U::Error, io::Error>>
    where
        U: Encoder,
    {
        let filter = self.filter();
        let mut buf = filter
            .get_write_buf()
            .unwrap_or_else(|| self.memory_pool().get_write_buf());

        let is_write_sleep = buf.is_empty();
        codec.encode(item, &mut buf).map_err(Either::Left)?;
        filter.release_write_buf(buf).map_err(Either::Right)?;
        if is_write_sleep {
            self.0.write_task.wake();
        }

        poll_fn(|cx| self.write().poll_write_ready(cx, true))
            .await
            .map_err(Either::Right)?;
        Ok(())
    }

    #[inline]
    /// Shut down connection
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let flags = self.flags();

        if flags.intersects(Flags::IO_ERR | Flags::IO_CLOSED) {
            Poll::Ready(Ok(()))
        } else {
            if !flags.contains(Flags::IO_FILTERS) {
                self.0.init_shutdown(Some(cx), self);
            }

            if let Some(err) = self.0.error.take() {
                Poll::Ready(Err(err))
            } else {
                self.0.dispatch_task.register(cx.waker());
                Poll::Pending
            }
        }
    }

    #[inline]
    /// Shut down connection
    pub async fn shutdown(&self) -> Result<(), io::Error> {
        poll_fn(|cx| self.poll_shutdown(cx)).await
    }

    #[inline]
    #[allow(clippy::type_complexity)]
    pub fn poll_next<U>(
        &self,
        codec: &U,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<U::Item>, Either<U::Error, io::Error>>>
    where
        U: Decoder,
    {
        let read = self.read();

        match read.decode(codec) {
            Ok(Some(el)) => Poll::Ready(Ok(Some(el))),
            Ok(None) => match read.poll_read_ready(cx) {
                Ok(()) => Poll::Pending,
                Err(Some(e)) => Poll::Ready(Err(Either::Right(e))),
                Err(None) => Poll::Ready(Ok(None)),
            },
            Err(err) => Poll::Ready(Err(Either::Left(err))),
        }
    }
}

impl fmt::Debug for IoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoRef")
            .field("open", &!self.is_closed())
            .finish()
    }
}

impl<F: Filter> Io<F> {
    #[inline]
    /// Get referece to filter
    pub fn filter(&self) -> &F {
        if let FilterItem::Ptr(p) = self.1 {
            if let Some(r) = unsafe { p.as_ref() } {
                return r;
            }
        }
        panic!()
    }

    #[inline]
    pub fn into_boxed(mut self) -> crate::IoBoxed
    where
        F: 'static,
    {
        // get current filter
        let filter = unsafe {
            let item = mem::replace(&mut self.1, FilterItem::Ptr(std::ptr::null_mut()));
            let filter: Box<dyn Filter> = match item {
                FilterItem::Boxed(b) => b,
                FilterItem::Ptr(p) => Box::new(*Box::from_raw(p)),
            };

            let filter_ref: &'static dyn Filter = {
                let filter: &dyn Filter = filter.as_ref();
                std::mem::transmute(filter)
            };
            self.0 .0.filter.replace(filter_ref);
            filter
        };

        Io(self.0.clone(), FilterItem::Boxed(filter))
    }

    #[inline]
    pub fn add_filter<T>(self, factory: T) -> T::Future
    where
        T: FilterFactory<F>,
    {
        factory.create(self)
    }

    #[inline]
    pub fn map_filter<T, U, E>(mut self, map: U) -> Result<Io<T>, E>
    where
        T: Filter,
        U: FnOnce(F) -> Result<T, E>,
    {
        // replace current filter
        let filter = unsafe {
            let item = mem::replace(&mut self.1, FilterItem::Ptr(std::ptr::null_mut()));
            let filter = match item {
                FilterItem::Boxed(_) => panic!(),
                FilterItem::Ptr(p) => {
                    assert!(!p.is_null());
                    Box::new(map(*Box::from_raw(p))?)
                }
            };
            let filter_ref: &'static dyn Filter = {
                let filter: &dyn Filter = filter.as_ref();
                std::mem::transmute(filter)
            };
            self.0 .0.filter.replace(filter_ref);
            filter
        };

        Ok(Io(self.0.clone(), FilterItem::Ptr(Box::into_raw(filter))))
    }
}

impl<F> Drop for Io<F> {
    fn drop(&mut self) {
        if let FilterItem::Ptr(p) = self.1 {
            if p.is_null() {
                return;
            }
            log::trace!(
                "io is dropped, force stopping io streams {:?}",
                self.0.flags()
            );

            self.force_close();
            self.0 .0.filter.set(NullFilter::get());
            let _ = mem::replace(&mut self.1, FilterItem::Ptr(std::ptr::null_mut()));
            unsafe { Box::from_raw(p) };
        } else {
            log::trace!(
                "io is dropped, force stopping io streams {:?}",
                self.0.flags()
            );
            self.force_close();
            self.0 .0.filter.set(NullFilter::get());
        }
    }
}

impl fmt::Debug for Io {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Io")
            .field("open", &!self.is_closed())
            .finish()
    }
}

impl<F> Deref for Io<F> {
    type Target = IoRef;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Copy, Clone)]
pub struct WriteRef<'a>(pub(super) &'a IoRef);

impl<'a> WriteRef<'a> {
    #[inline]
    /// Check if write task is ready
    pub fn is_ready(&self) -> bool {
        !self.0 .0.flags.get().contains(Flags::WR_BACKPRESSURE)
    }

    #[inline]
    /// Check if write buffer is full
    pub fn is_full(&self) -> bool {
        if let Some(buf) = self.0 .0.read_buf.take() {
            let hw = self.0.memory_pool().write_params_high();
            let result = buf.len() >= hw;
            self.0 .0.write_buf.set(Some(buf));
            result
        } else {
            false
        }
    }

    #[inline]
    /// Wake dispatcher task
    pub fn wake_dispatcher(&self) {
        self.0 .0.dispatch_task.wake();
    }

    #[inline]
    /// Wait until write task flushes data to io stream
    ///
    /// Write task must be waken up separately.
    pub fn enable_backpressure(&self, cx: Option<&mut Context<'_>>) {
        log::trace!("enable write back-pressure {:?}", cx.is_some());
        self.0 .0.insert_flags(Flags::WR_BACKPRESSURE);
        if let Some(cx) = cx {
            self.0 .0.dispatch_task.register(cx.waker());
        }
    }

    #[inline]
    /// Get mut access to write buffer
    pub fn with_buf<F, R>(&self, f: F) -> Result<R, io::Error>
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let filter = self.0.filter();
        let mut buf = filter
            .get_write_buf()
            .unwrap_or_else(|| self.0.memory_pool().get_write_buf());
        if buf.is_empty() {
            self.0 .0.write_task.wake();
        }

        let result = f(&mut buf);
        filter.release_write_buf(buf)?;
        Ok(result)
    }

    #[inline]
    /// Encode and write item to a buffer and wake up write task
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
        let flags = self.0 .0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            let filter = self.0.filter();
            let mut buf = filter
                .get_write_buf()
                .unwrap_or_else(|| self.0.memory_pool().get_write_buf());
            let is_write_sleep = buf.is_empty();
            let (hw, lw) = self.0.memory_pool().write_params().unpack();

            // make sure we've got room
            let remaining = buf.capacity() - buf.len();
            if remaining < lw {
                buf.reserve(hw - remaining);
            }

            // encode item and wake write task
            let result = codec.encode(item, &mut buf).map(|_| {
                if is_write_sleep {
                    self.0 .0.write_task.wake();
                }
                buf.len() < hw
            });
            if let Err(err) = filter.release_write_buf(buf) {
                self.0 .0.set_error(Some(err));
            }
            result
        } else {
            Ok(true)
        }
    }

    #[inline]
    /// Write bytes to a buffer and wake up write task
    ///
    /// Returns write buffer state, false is returned if write buffer if full.
    pub fn write(&self, src: &[u8]) -> Result<bool, io::Error> {
        let flags = self.0 .0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            let filter = self.0.filter();
            let mut buf = filter
                .get_write_buf()
                .unwrap_or_else(|| self.0.memory_pool().get_write_buf());
            let is_write_sleep = buf.is_empty();

            // write and wake write task
            buf.extend_from_slice(src);
            let result = buf.len() < self.0.memory_pool().write_params_high();
            if is_write_sleep {
                self.0 .0.write_task.wake();
            }

            if let Err(err) = filter.release_write_buf(buf) {
                self.0 .0.set_error(Some(err));
            }
            Ok(result)
        } else {
            Ok(true)
        }
    }

    #[inline]
    /// Wake write task and instruct to write data.
    ///
    /// If full is true then wake up dispatcher when all data is flushed
    /// otherwise wake up when size of write buffer is lower than
    /// buffer max size.
    pub fn poll_write_ready(
        &self,
        cx: &mut Context<'_>,
        full: bool,
    ) -> Poll<Result<(), io::Error>> {
        // check io error
        if !self.0 .0.is_io_open() {
            return Poll::Ready(Err(self.0 .0.error.take().unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "disconnected")
            })));
        }

        if let Some(buf) = self.0 .0.write_buf.take() {
            let len = buf.len();
            if len != 0 {
                self.0 .0.write_buf.set(Some(buf));

                if full {
                    self.0 .0.insert_flags(Flags::WR_WAIT);
                    self.0 .0.dispatch_task.register(cx.waker());
                    return Poll::Pending;
                } else if len >= self.0.memory_pool().write_params_high() << 1 {
                    self.0 .0.insert_flags(Flags::WR_BACKPRESSURE);
                    self.0 .0.dispatch_task.register(cx.waker());
                    return Poll::Pending;
                } else {
                    self.0 .0.remove_flags(Flags::WR_BACKPRESSURE);
                }
            }
        }

        Poll::Ready(Ok(()))
    }

    #[inline]
    /// Wake write task and instruct to write data.
    ///
    /// This is async version of .poll_write_ready() method.
    pub async fn write_ready(&self, full: bool) -> Result<(), io::Error> {
        poll_fn(|cx| self.poll_write_ready(cx, full)).await
    }
}

#[derive(Copy, Clone)]
pub struct ReadRef<'a>(&'a IoRef);

impl<'a> ReadRef<'a> {
    #[inline]
    /// Check if read buffer has new data
    pub fn is_ready(&self) -> bool {
        self.0 .0.flags.get().contains(Flags::RD_READY)
    }

    /// Reset readiness state, returns previous state
    pub fn take_readiness(&self) -> bool {
        let mut flags = self.0 .0.flags.get();
        let ready = flags.contains(Flags::RD_READY);
        flags.remove(Flags::RD_READY);
        self.0 .0.flags.set(flags);
        ready
    }

    #[inline]
    /// Check if read buffer is full
    pub fn is_full(&self) -> bool {
        if let Some(buf) = self.0 .0.read_buf.take() {
            let result = buf.len() >= self.0.memory_pool().read_params_high();
            self.0 .0.read_buf.set(Some(buf));
            result
        } else {
            false
        }
    }

    #[inline]
    /// Pause read task
    pub fn pause(&self, cx: &mut Context<'_>) {
        self.0 .0.insert_flags(Flags::RD_PAUSED);
        self.0 .0.dispatch_task.register(cx.waker());
    }

    #[inline]
    /// Wake read io task if it is paused
    pub fn resume(&self) -> bool {
        let flags = self.0 .0.flags.get();
        if flags.contains(Flags::RD_PAUSED) {
            self.0 .0.remove_flags(Flags::RD_PAUSED);
            self.0 .0.read_task.wake();
            true
        } else {
            false
        }
    }

    #[inline]
    /// Attempts to decode a frame from the read buffer
    ///
    /// Read buffer ready state gets cleanup if decoder cannot
    /// decode any frame.
    pub fn decode<U>(
        &self,
        codec: &U,
    ) -> Result<Option<<U as Decoder>::Item>, <U as Decoder>::Error>
    where
        U: Decoder,
    {
        if let Some(mut buf) = self.0 .0.read_buf.take() {
            let result = codec.decode(&mut buf);
            self.0 .0.read_buf.set(Some(buf));
            return result;
        }
        Ok(None)
    }

    #[inline]
    /// Get mut access to read buffer
    pub fn with_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let mut buf = self
            .0
             .0
            .read_buf
            .take()
            .unwrap_or_else(|| self.0.memory_pool().get_read_buf());
        let res = f(&mut buf);
        if buf.is_empty() {
            self.0.memory_pool().release_read_buf(buf);
        } else {
            self.0 .0.read_buf.set(Some(buf));
        }
        res
    }

    #[inline]
    /// Wake read task and instruct to read more data
    ///
    /// Read task is awake only if back-pressure is enabled
    /// otherwise it is already awake. Buffer read status gets clean up.
    pub fn poll_read_ready(
        &self,
        cx: &mut Context<'_>,
    ) -> Result<(), Option<io::Error>> {
        let mut flags = self.0 .0.flags.get();

        if !self.0 .0.is_io_open() {
            Err(self.0 .0.error.take())
        } else {
            if flags.contains(Flags::RD_BUF_FULL) {
                log::trace!("read back-pressure is disabled, wake io task");
                flags.remove(Flags::RD_READY | Flags::RD_BUF_FULL);
                self.0 .0.flags.set(flags);
                self.0 .0.read_task.wake();
            } else if flags.contains(Flags::RD_READY) {
                log::trace!("waking up io read task");
                flags.remove(Flags::RD_READY);
                self.0 .0.flags.set(flags);
                self.0 .0.read_task.wake();
            }
            self.0 .0.dispatch_task.register(cx.waker());
            Ok(())
        }
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
        let token = if disconnected {
            usize::MAX
        } else {
            let mut on_disconnect = inner.on_disconnect.borrow_mut();
            let token = on_disconnect.len();
            on_disconnect.push(Some(LocalWaker::default()));
            drop(on_disconnect);
            token
        };
        Self { token, inner }
    }

    #[inline]
    /// Check if connection is disconnected
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.token == usize::MAX {
            Poll::Ready(())
        } else {
            let on_disconnect = self.inner.on_disconnect.borrow();
            if on_disconnect[self.token].is_some() {
                on_disconnect[self.token]
                    .as_ref()
                    .unwrap()
                    .register(cx.waker());
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }
}

impl Clone for OnDisconnect {
    fn clone(&self) -> Self {
        if self.token == usize::MAX {
            OnDisconnect::new(self.inner.clone(), true)
        } else {
            OnDisconnect::new(self.inner.clone(), false)
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

impl Drop for OnDisconnect {
    fn drop(&mut self) {
        if self.token != usize::MAX {
            self.inner.on_disconnect.borrow_mut()[self.token].take();
        }
    }
}

#[cfg(test)]
mod tests {
    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;
    use ntex_util::future::{lazy, Ready};
    use ntex_util::time::{sleep, Millis};

    use super::*;
    use crate::testing::IoTest;
    use crate::{Filter, FilterFactory, WriteReadiness};

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[ntex::test]
    async fn utils() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write(TEXT);

        let state = Io::new(server);
        assert!(!state.read().is_full());
        assert!(!state.write().is_full());

        let msg = state.next(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        let res = poll_fn(|cx| Poll::Ready(state.poll_next(&BytesCodec, cx))).await;
        assert!(res.is_pending());
        client.write(TEXT);
        sleep(Millis(50)).await;
        let res = poll_fn(|cx| Poll::Ready(state.poll_next(&BytesCodec, cx))).await;
        if let Poll::Ready(msg) = res {
            assert_eq!(msg.unwrap().unwrap(), Bytes::from_static(BIN));
        }

        client.read_error(io::Error::new(io::ErrorKind::Other, "err"));
        let msg = state.next(&BytesCodec).await;
        assert!(msg.is_err());
        assert!(state.flags().contains(Flags::IO_ERR));
        assert!(state.flags().contains(Flags::DSP_STOP));

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::new(server);

        client.read_error(io::Error::new(io::ErrorKind::Other, "err"));
        let res = poll_fn(|cx| Poll::Ready(state.poll_next(&BytesCodec, cx))).await;
        if let Poll::Ready(msg) = res {
            assert!(msg.is_err());
            assert!(state.flags().contains(Flags::IO_ERR));
            assert!(state.flags().contains(Flags::DSP_STOP));
        }

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::new(server);
        state
            .send(Bytes::from_static(b"test"), &BytesCodec)
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write_error(io::Error::new(io::ErrorKind::Other, "err"));
        let res = state.send(Bytes::from_static(b"test"), &BytesCodec).await;
        assert!(res.is_err());
        assert!(state.flags().contains(Flags::IO_ERR));
        assert!(state.flags().contains(Flags::DSP_STOP));

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::new(server);
        state.force_close();
        assert!(state.flags().contains(Flags::DSP_STOP));
        assert!(state.flags().contains(Flags::IO_SHUTDOWN));
    }

    #[ntex::test]
    async fn on_disconnect() {
        let (client, server) = IoTest::create();
        let state = Io::new(server);
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
        client.close().await;
        assert_eq!(waiter.await, ());
        assert_eq!(waiter2.await, ());

        let mut waiter = state.on_disconnect();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Ready(())
        );

        let (client, server) = IoTest::create();
        let state = Io::new(server);
        let mut waiter = state.on_disconnect();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Pending
        );
        client.read_error(io::Error::new(io::ErrorKind::Other, "err"));
        assert_eq!(waiter.await, ());
    }

    struct Counter<F> {
        inner: F,
        in_bytes: Rc<Cell<usize>>,
        out_bytes: Rc<Cell<usize>>,
    }
    impl<F: Filter> Filter for Counter<F> {
        fn shutdown(&self, _: &IoRef) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn query(&self, _: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
            None
        }

        fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
            self.inner.poll_read_ready(cx)
        }

        fn closed(&self, err: Option<io::Error>) {
            self.inner.closed(err)
        }

        fn get_read_buf(&self) -> Option<BytesMut> {
            self.inner.get_read_buf()
        }

        fn release_read_buf(
            &self,
            buf: BytesMut,
            new_bytes: usize,
        ) -> Result<(), io::Error> {
            self.in_bytes.set(self.in_bytes.get() + new_bytes);
            self.inner.release_read_buf(buf, new_bytes)
        }

        fn poll_write_ready(
            &self,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), WriteReadiness>> {
            self.inner.poll_write_ready(cx)
        }

        fn get_write_buf(&self) -> Option<BytesMut> {
            if let Some(buf) = self.inner.get_write_buf() {
                self.out_bytes.set(self.out_bytes.get() - buf.len());
                Some(buf)
            } else {
                None
            }
        }

        fn release_write_buf(&self, buf: BytesMut) -> Result<(), io::Error> {
            self.out_bytes.set(self.out_bytes.get() + buf.len());
            self.inner.release_write_buf(buf)
        }
    }

    struct CounterFactory(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl<F: Filter> FilterFactory<F> for CounterFactory {
        type Filter = Counter<F>;

        type Error = ();
        type Future = Ready<Io<Counter<F>>, Self::Error>;

        fn create(self, io: Io<F>) -> Self::Future {
            let in_bytes = self.0.clone();
            let out_bytes = self.1.clone();
            Ready::Ok(
                io.map_filter(|inner| {
                    Ok::<_, ()>(Counter {
                        inner,
                        in_bytes,
                        out_bytes,
                    })
                })
                .unwrap(),
            )
        }
    }

    #[ntex::test]
    async fn filter() {
        let in_bytes = Rc::new(Cell::new(0));
        let out_bytes = Rc::new(Cell::new(0));
        let factory = CounterFactory(in_bytes.clone(), out_bytes.clone());

        let (client, server) = IoTest::create();
        let state = Io::new(server).add_filter(factory).await.unwrap();

        client.remote_buffer_cap(1024);
        client.write(TEXT);
        let msg = state.next(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        state
            .send(Bytes::from_static(b"test"), &BytesCodec)
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        assert_eq!(in_bytes.get(), BIN.len());
        assert_eq!(out_bytes.get(), 4);
    }

    #[ntex::test]
    async fn boxed_filter() {
        let in_bytes = Rc::new(Cell::new(0));
        let out_bytes = Rc::new(Cell::new(0));

        let (client, server) = IoTest::create();
        let state = Io::new(server)
            .add_filter(CounterFactory(in_bytes.clone(), out_bytes.clone()))
            .await
            .unwrap()
            .add_filter(CounterFactory(in_bytes.clone(), out_bytes.clone()))
            .await
            .unwrap();
        let state = state.into_boxed();

        client.remote_buffer_cap(1024);
        client.write(TEXT);
        let msg = state.next(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        state
            .send(Bytes::from_static(b"test"), &BytesCodec)
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        assert_eq!(in_bytes.get(), BIN.len() * 2);
        assert_eq!(out_bytes.get(), 8);

        // refs
        assert_eq!(Rc::strong_count(&in_bytes), 3);
        drop(state);
        assert_eq!(Rc::strong_count(&in_bytes), 1);
    }
}
