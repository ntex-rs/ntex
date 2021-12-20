use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{fmt, future::Future, hash, io, mem, ops::Deref, pin::Pin, ptr, rc::Rc};

use ntex_bytes::{BytesMut, PoolId, PoolRef};
use ntex_codec::{Decoder, Encoder};
use ntex_util::{future::poll_fn, future::Either, task::LocalWaker, time::Millis};

use super::filter::{Base, NullFilter};
use super::tasks::{ReadContext, WriteContext};
use super::{Filter, FilterFactory, Handle, IoStream};

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

pub struct Io<F = Base>(pub(super) IoRef, FilterItem<F>);

#[derive(Clone)]
pub struct IoRef(pub(super) Rc<IoState>);

pub(crate) struct IoState {
    pub(super) flags: Cell<Flags>,
    pub(super) pool: Cell<PoolRef>,
    pub(super) disconnect_timeout: Cell<Millis>,
    pub(super) error: Cell<Option<io::Error>>,
    pub(super) read_task: LocalWaker,
    pub(super) write_task: LocalWaker,
    pub(super) dispatch_task: LocalWaker,
    pub(super) read_buf: Cell<Option<BytesMut>>,
    pub(super) write_buf: Cell<Option<BytesMut>>,
    pub(super) filter: Cell<&'static dyn Filter>,
    pub(super) handle: Cell<Option<Box<dyn Handle>>>,
    pub(super) on_disconnect: RefCell<Vec<Option<LocalWaker>>>,
}

impl IoState {
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
    pub(super) fn is_io_open(&self) -> bool {
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
        let flags = self.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN | Flags::IO_FILTERS) {
            log::trace!("initiate io shutdown {:?}", flags);
            self.insert_flags(Flags::IO_FILTERS);
            if let Err(err) = self.shutdown_filters(st) {
                self.error.set(Some(err));
            }

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
        let inner = Rc::new(IoState {
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

        let filter = Box::new(Base::new(IoRef(inner.clone())));
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
    #[doc(hidden)]
    /// Get current state flags
    pub fn flags(&self) -> Flags {
        self.0 .0.flags.get()
    }

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
    /// Check if dispatcher marked stopped
    pub fn is_dispatcher_stopped(&self) -> bool {
        self.flags().contains(Flags::DSP_STOP)
    }

    #[inline]
    /// Register dispatcher task
    pub fn register_dispatcher(&self, cx: &mut Context<'_>) {
        self.0 .0.dispatch_task.register(cx.waker());
    }

    #[inline]
    /// Reset keep-alive error
    pub fn reset_keepalive(&self) {
        self.0 .0.remove_flags(Flags::DSP_KEEPALIVE)
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

impl<F> Io<F> {
    #[inline]
    /// Read incoming io stream and decode codec item.
    pub async fn next<U>(
        &self,
        codec: &U,
    ) -> Option<Result<U::Item, Either<U::Error, io::Error>>>
    where
        U: Decoder,
    {
        poll_fn(|cx| self.poll_read_next(codec, cx)).await
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
            self.0 .0.write_task.wake();
        }

        poll_fn(|cx| self.poll_write_ready(cx, true))
            .await
            .map_err(Either::Right)?;
        Ok(())
    }

    #[inline]
    /// Wake write task and instruct to write data.
    ///
    /// This is async version of .poll_write_ready() method.
    pub async fn write_ready(&self, full: bool) -> Result<(), io::Error> {
        poll_fn(|cx| self.poll_write_ready(cx, full)).await
    }

    #[inline]
    /// Shut down connection
    pub async fn shutdown(&self) -> Result<(), io::Error> {
        poll_fn(|cx| self.poll_shutdown(cx)).await
    }
}

impl<F> Io<F> {
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
    ) -> Poll<io::Result<()>> {
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
    /// Wake read task and instruct to read more data
    ///
    /// Read task is awake only if back-pressure is enabled
    /// otherwise it is already awake. Buffer read status gets clean up.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Option<io::Result<()>>> {
        if !self.0 .0.is_io_open() {
            if let Some(err) = self.0 .0.error.take() {
                Poll::Ready(Some(Err(err)))
            } else {
                Poll::Ready(None)
            }
        } else {
            self.0 .0.dispatch_task.register(cx.waker());

            let mut flags = self.0 .0.flags.get();
            let ready = flags.contains(Flags::RD_READY);
            if flags.contains(Flags::RD_BUF_FULL) {
                log::trace!("read back-pressure is disabled, wake io task");
                flags.remove(Flags::RD_READY | Flags::RD_BUF_FULL);
                self.0 .0.read_task.wake();
                self.0 .0.flags.set(flags);
                if ready {
                    Poll::Ready(Some(Ok(())))
                } else {
                    Poll::Pending
                }
            } else if ready {
                log::trace!("waking up io read task");
                flags.remove(Flags::RD_READY);
                self.0 .0.flags.set(flags);
                self.0 .0.read_task.wake();
                Poll::Ready(Some(Ok(())))
            } else {
                Poll::Pending
            }
        }
    }

    #[inline]
    #[allow(clippy::type_complexity)]
    pub fn poll_read_next<U>(
        &self,
        codec: &U,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<U::Item, Either<U::Error, io::Error>>>>
    where
        U: Decoder,
    {
        match self.decode(codec) {
            Ok(Some(el)) => Poll::Ready(Some(Ok(el))),
            Ok(None) => match self.poll_read_ready(cx) {
                Poll::Pending | Poll::Ready(Some(Ok(()))) => Poll::Pending,
                Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Either::Right(e)))),
                Poll::Ready(None) => Poll::Ready(None),
            },
            Err(err) => Poll::Ready(Some(Err(Either::Left(err)))),
        }
    }

    #[inline]
    /// Shut down connection
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let flags = self.flags();

        if flags.intersects(Flags::IO_ERR | Flags::IO_CLOSED) {
            Poll::Ready(Ok(()))
        } else {
            if !flags.contains(Flags::IO_FILTERS) {
                self.0 .0.init_shutdown(Some(cx), self.as_ref());
            }

            if let Some(err) = self.0 .0.error.take() {
                Poll::Ready(Err(err))
            } else {
                self.0 .0.dispatch_task.register(cx.waker());
                Poll::Pending
            }
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
    /// Wait until write task flushes data to io stream
    ///
    /// Write task must be waken up separately.
    pub fn enable_write_backpressure(&self, cx: &mut Context<'_>) {
        log::trace!("enable write back-pressure for dispatcher");
        self.0 .0.insert_flags(Flags::WR_BACKPRESSURE);
        self.0 .0.dispatch_task.register(cx.waker());
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

/// OnDisconnect future resolves when socket get disconnected
#[must_use = "OnDisconnect do nothing unless polled"]
pub struct OnDisconnect {
    token: usize,
    inner: Rc<IoState>,
}

impl OnDisconnect {
    pub(super) fn new(inner: Rc<IoState>) -> Self {
        Self::new_inner(inner.flags.get().contains(Flags::IO_ERR), inner)
    }

    fn new_inner(disconnected: bool, inner: Rc<IoState>) -> Self {
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

impl Drop for OnDisconnect {
    fn drop(&mut self) {
        if self.token != usize::MAX {
            self.inner.on_disconnect.borrow_mut()[self.token].take();
        }
    }
}
