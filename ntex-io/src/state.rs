use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{future::Future, hash, io, mem, ops::Deref, pin::Pin, ptr, rc::Rc};

use ntex_bytes::{BytesMut, PoolId, PoolRef};
use ntex_codec::{Decoder, Encoder};
use ntex_util::time::{Millis, Seconds};
use ntex_util::{future::poll_fn, future::Either, task::LocalWaker};

use super::filter::{DefaultFilter, NullFilter};
use super::tasks::{ReadState, WriteState};
use super::{Filter, FilterFactory, IoStream};

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

        /// pause io read
        const RD_PAUSED       = 0b0000_0000_0000_1000;
        /// new data is available
        const RD_READY        = 0b0000_0000_0001_0000;
        /// read buffer is full
        const RD_BUF_FULL     = 0b0000_0000_0010_0000;

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
    pub(super) filter: Cell<&'static dyn Filter>,
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
    fn is_io_err(&self) -> bool {
        self.flags.get().contains(Flags::IO_ERR)
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
            on_disconnect: RefCell::new(Vec::new()),
        });

        let filter = Box::new(DefaultFilter::new(inner.clone()));
        let filter_ref: &'static dyn Filter = unsafe {
            let filter: &dyn Filter = filter.as_ref();
            std::mem::transmute(filter)
        };
        inner.filter.replace(filter_ref);

        let io_ref = IoRef(inner);

        // start io tasks
        io.start(ReadState(io_ref.clone()), WriteState(io_ref.clone()));

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
    pub fn set_disconnect_timeout(&self, timeout: Seconds) {
        self.0 .0.disconnect_timeout.set(timeout.into());
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

    #[inline]
    /// Mark dispatcher as stopped
    pub fn dispatcher_stopped(&self) {
        self.0 .0.insert_flags(Flags::DSP_STOP);
    }

    #[inline]
    /// Gracefully shutdown read and write io tasks
    pub fn init_shutdown(&self, cx: &mut Context<'_>) {
        let flags = self.0 .0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN | Flags::IO_FILTERS) {
            log::trace!("initiate io shutdown {:?}", flags);
            self.0 .0.insert_flags(Flags::IO_FILTERS);
            if let Err(err) = self.0 .0.shutdown_filters(&self.0) {
                self.0 .0.error.set(Some(err));
                self.0 .0.insert_flags(Flags::IO_ERR);
            }

            self.0 .0.read_task.wake();
            self.0 .0.write_task.wake();
            self.0 .0.dispatch_task.register(cx.waker());
        }
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
    /// Get memory pool
    pub fn memory_pool(&self) -> PoolRef {
        self.0.pool.get()
    }

    #[inline]
    /// Check if io error occured in read or write task
    pub fn is_io_err(&self) -> bool {
        self.0.is_io_err()
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
        self.0
            .flags
            .get()
            .intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN | Flags::DSP_STOP)
    }

    #[inline]
    /// Take io error if any occured
    pub fn take_error(&self) -> Option<io::Error> {
        self.0.error.take()
    }

    #[inline]
    /// Reset keep-alive error
    pub fn reset_keepalive(&self) {
        self.0.remove_flags(Flags::DSP_KEEPALIVE)
    }

    #[inline]
    /// Get api for read task
    pub fn read(&'_ self) -> ReadRef<'_> {
        ReadRef(self.0.as_ref())
    }

    #[inline]
    /// Get api for write task
    pub fn write(&'_ self) -> WriteRef<'_> {
        WriteRef(self.0.as_ref())
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
}

impl<F> Io<F> {
    #[inline]
    /// Read incoming io stream and decode codec item.
    pub async fn next<U>(
        &self,
        codec: &U,
    ) -> Result<Option<U::Item>, Either<U::Error, io::Error>>
    where
        U: Decoder,
    {
        let read = self.read();

        loop {
            let mut buf = self.0 .0.read_buf.take();
            let item = if let Some(ref mut buf) = buf {
                codec.decode(buf)
            } else {
                Ok(None)
            };
            self.0 .0.read_buf.set(buf);

            return match item {
                Ok(Some(el)) => Ok(Some(el)),
                Ok(None) => {
                    self.0 .0.remove_flags(Flags::RD_READY);
                    if poll_fn(|cx| read.poll_ready(cx))
                        .await
                        .map_err(Either::Right)?
                        .is_none()
                    {
                        return Ok(None);
                    }
                    continue;
                }
                Err(err) => Err(Either::Left(err)),
            };
        }
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
        let filter = self.0 .0.filter.get();
        let mut buf = filter
            .get_write_buf()
            .unwrap_or_else(|| self.0 .0.pool.get().get_write_buf());

        let is_write_sleep = buf.is_empty();
        codec.encode(item, &mut buf).map_err(Either::Left)?;
        filter.release_write_buf(buf).map_err(Either::Right)?;
        self.0 .0.insert_flags(Flags::WR_WAIT);
        if is_write_sleep {
            self.0 .0.write_task.wake();
        }

        poll_fn(|cx| self.write().poll_flush(cx))
            .await
            .map_err(Either::Right)?;
        Ok(())
    }

    #[inline]
    /// Shuts down connection
    pub async fn shutdown(&self) -> Result<(), io::Error> {
        if self.flags().intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            Ok(())
        } else {
            poll_fn(|cx| {
                let flags = self.flags();
                if !flags.contains(Flags::IO_FILTERS) {
                    self.init_shutdown(cx);
                }

                if self.flags().intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
                    if let Some(err) = self.0 .0.error.take() {
                        Poll::Ready(Err(err))
                    } else {
                        Poll::Ready(Ok(()))
                    }
                } else {
                    self.0 .0.insert_flags(Flags::IO_FILTERS);
                    self.0 .0.dispatch_task.register(cx.waker());
                    Poll::Pending
                }
            })
            .await
        }
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
        if self
            .read()
            .poll_ready(cx)
            .map_err(Either::Right)?
            .is_ready()
        {
            let mut buf = self.0 .0.read_buf.take();
            let item = if let Some(ref mut buf) = buf {
                codec.decode(buf)
            } else {
                Ok(None)
            };
            self.0 .0.read_buf.set(buf);

            match item {
                Ok(Some(el)) => Poll::Ready(Ok(Some(el))),
                Ok(None) => {
                    if let Poll::Ready(res) =
                        self.read().poll_ready(cx).map_err(Either::Right)?
                    {
                        if res.is_none() {
                            return Poll::Ready(Ok(None));
                        }
                    }
                    Poll::Pending
                }
                Err(err) => Poll::Ready(Err(Either::Left(err))),
            }
        } else {
            Poll::Pending
        }
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
    pub fn map_filter<T, U>(mut self, map: U) -> Result<Io<T::Filter>, T::Error>
    where
        T: FilterFactory<F>,
        U: FnOnce(F) -> Result<T::Filter, T::Error>,
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
        log::trace!("stopping io stream");
        if let FilterItem::Ptr(p) = self.1 {
            if p.is_null() {
                return;
            }
            self.force_close();
            self.0 .0.filter.set(NullFilter::get());
            let _ = mem::replace(&mut self.1, FilterItem::Ptr(std::ptr::null_mut()));
            unsafe { Box::from_raw(p) };
        } else {
            self.force_close();
            self.0 .0.filter.set(NullFilter::get());
        }
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
pub struct WriteRef<'a>(pub(super) &'a IoStateInner);

impl<'a> WriteRef<'a> {
    #[inline]
    /// Check if write task is ready
    pub fn is_ready(&self) -> bool {
        !self.0.flags.get().contains(Flags::WR_BACKPRESSURE)
    }

    #[inline]
    /// Check if write buffer is full
    pub fn is_full(&self) -> bool {
        if let Some(buf) = self.0.read_buf.take() {
            let hw = self.0.pool.get().write_params_high();
            let result = buf.len() >= hw;
            self.0.write_buf.set(Some(buf));
            result
        } else {
            false
        }
    }

    #[inline]
    /// Wake dispatcher task
    pub fn wake_dispatcher(&self) {
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Wait until write task flushes data to io stream
    ///
    /// Write task must be waken up separately.
    pub fn enable_backpressure(&self, cx: Option<&mut Context<'_>>) {
        log::trace!("enable write back-pressure");
        self.0.insert_flags(Flags::WR_BACKPRESSURE);
        if let Some(cx) = cx {
            self.0.dispatch_task.register(cx.waker());
        }
    }

    #[inline]
    /// Get mut access to write buffer
    pub fn with_buf<F, R>(&self, f: F) -> Result<R, io::Error>
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let filter = self.0.filter.get();
        let mut buf = filter
            .get_write_buf()
            .unwrap_or_else(|| self.0.pool.get().get_write_buf());
        if buf.is_empty() {
            self.0.write_task.wake();
        }

        let result = f(&mut buf);
        filter.release_write_buf(buf)?;
        Ok(result)
    }

    #[inline]
    /// Write item to a buffer and wake up write task
    ///
    /// Returns write buffer state, false is returned if write buffer if full.
    pub fn encode<U>(
        &self,
        item: U::Item,
        codec: &U,
    ) -> Result<bool, Either<<U as Encoder>::Error, io::Error>>
    where
        U: Encoder,
    {
        let flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            let filter = self.0.filter.get();
            let mut buf = filter
                .get_write_buf()
                .unwrap_or_else(|| self.0.pool.get().get_write_buf());
            let is_write_sleep = buf.is_empty();
            let (hw, lw) = self.0.pool.get().write_params().unpack();

            // make sure we've got room
            let remaining = buf.capacity() - buf.len();
            if remaining < lw {
                buf.reserve(hw - remaining);
            }

            // encode item and wake write task
            let result = codec
                .encode(item, &mut buf)
                .map(|_| {
                    if is_write_sleep {
                        self.0.write_task.wake();
                    }
                    buf.len() < hw
                })
                .map_err(Either::Left);
            filter.release_write_buf(buf).map_err(Either::Right)?;
            Ok(result?)
        } else {
            Ok(true)
        }
    }

    #[inline]
    /// Wake write task and instruct to write all data.
    ///
    /// When write task is done wake dispatcher.
    pub fn poll_flush(&self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.0.insert_flags(Flags::WR_WAIT);

        if let Some(buf) = self.0.write_buf.take() {
            if !buf.is_empty() {
                self.0.write_buf.set(Some(buf));
                self.0.write_task.wake();
                self.0.dispatch_task.register(cx.waker());
                return Poll::Pending;
            }
        }

        if self.0.is_io_err() {
            Poll::Ready(Err(self.0.error.take().unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "disconnected")
            })))
        } else {
            self.0.dispatch_task.register(cx.waker());
            Poll::Ready(Ok(()))
        }
    }
}

#[derive(Copy, Clone)]
pub struct ReadRef<'a>(&'a IoStateInner);

impl<'a> ReadRef<'a> {
    #[inline]
    /// Check if read buffer has new data
    pub fn is_ready(&self) -> bool {
        self.0.flags.get().contains(Flags::RD_READY)
    }

    #[inline]
    /// Check if read buffer is full
    pub fn is_full(&self) -> bool {
        if let Some(buf) = self.0.read_buf.take() {
            let result = buf.len() >= self.0.pool.get().read_params_high();
            self.0.read_buf.set(Some(buf));
            result
        } else {
            false
        }
    }

    #[inline]
    /// Pause read task
    ///
    /// Also register dispatch task
    pub fn pause(&self, cx: &mut Context<'_>) {
        self.0.insert_flags(Flags::RD_PAUSED);
        self.0.dispatch_task.register(cx.waker());
    }

    #[inline]
    /// Wake read io task if it is paused
    pub fn resume(&self) -> bool {
        let flags = self.0.flags.get();
        if flags.contains(Flags::RD_PAUSED) {
            self.0.remove_flags(Flags::RD_PAUSED);
            self.0.read_task.wake();
            true
        } else {
            false
        }
    }

    #[inline]
    /// Attempts to decode a frame from the read buffer.
    pub fn decode<U>(
        &self,
        codec: &U,
    ) -> Result<Option<<U as Decoder>::Item>, <U as Decoder>::Error>
    where
        U: Decoder,
    {
        let mut buf = self.0.read_buf.take();
        let result = if let Some(ref mut buf) = buf {
            let result = codec.decode(buf);
            if result.as_ref().map(|v| v.is_none()).unwrap_or(false) {
                self.0.remove_flags(Flags::RD_READY);
            }
            result
        } else {
            self.0.remove_flags(Flags::RD_READY);
            Ok(None)
        };
        self.0.read_buf.set(buf);
        result
    }

    #[inline]
    /// Get mut access to read buffer
    pub fn with_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let mut buf = self
            .0
            .read_buf
            .take()
            .unwrap_or_else(|| self.0.pool.get().get_read_buf());
        let res = f(&mut buf);
        if buf.is_empty() {
            self.0.remove_flags(Flags::RD_READY);
            self.0.pool.get().release_read_buf(buf);
        } else {
            self.0.read_buf.set(Some(buf));
        }
        res
    }

    #[inline]
    /// Wake read task and instruct to read more data
    ///
    /// Only wakes if back-pressure is enabled on read task
    /// otherwise read is already awake.
    pub fn poll_ready(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<()>, io::Error>> {
        let mut flags = self.0.flags.get();
        let ready = flags.contains(Flags::RD_READY);

        if self.0.is_io_err() {
            if let Some(err) = self.0.error.take() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(None))
            }
        } else if ready {
            Poll::Ready(Ok(Some(())))
        } else {
            flags.remove(Flags::RD_READY);
            if flags.contains(Flags::RD_BUF_FULL) {
                log::trace!("read back-pressure is enabled, wake io task");
                flags.remove(Flags::RD_BUF_FULL);
                self.0.read_task.wake();
            }
            self.0.flags.set(flags);
            self.0.dispatch_task.register(cx.waker());
            Poll::Pending
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

    use super::*;
    use crate::testing::IoTest;
    use crate::{Filter, FilterFactory, ReadFilter, WriteFilter, WriteReadiness};

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[ntex::test]
    async fn utils() {
        env_logger::init();
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
    impl<F: ReadFilter + WriteFilter> Filter for Counter<F> {
        fn shutdown(&self, _: &IoRef) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl<F: ReadFilter> ReadFilter for Counter<F> {
        fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
            self.inner.poll_read_ready(cx)
        }

        fn read_closed(&self, err: Option<io::Error>) {
            self.inner.read_closed(err)
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
    }

    impl<F: WriteFilter> WriteFilter for Counter<F> {
        fn poll_write_ready(
            &self,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), WriteReadiness>> {
            self.inner.poll_write_ready(cx)
        }

        fn write_closed(&self, err: Option<io::Error>) {
            self.inner.write_closed(err)
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
                io.map_filter::<CounterFactory, _>(|inner| {
                    Ok(Counter {
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
