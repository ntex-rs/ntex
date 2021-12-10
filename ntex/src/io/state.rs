use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{future::Future, hash, io, mem, pin::Pin, ptr, rc::Rc};

use slab::Slab;

use crate::codec::{Decoder, Encoder};
use crate::task::LocalWaker;
use crate::time::Seconds;
use crate::util::{poll_fn, BytesMut, Either, PoolId, PoolRef};

use super::filter::{DefaultFilter, NullFilter};
use super::tasks::{ReadState, WriteState};
use super::{Filter, FilterFactory, IoStream};

bitflags::bitflags! {
    pub struct Flags: u16 {
        /// io error occured
        const IO_ERR          = 0b0000_0000_0000_0001;
        /// stop io tasks
        const IO_STOP         = 0b0000_0000_0000_0010;
        /// shutdown io tasks
        const IO_SHUTDOWN     = 0b0000_0000_0000_0100;

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

pub struct IoState<F = DefaultFilter>(pub(super) Rc<IoStateInner>, *mut F);

pub(crate) struct IoStateInner {
    pub(super) flags: Cell<Flags>,
    pub(super) pool: Cell<PoolRef>,
    pub(super) disconnect_timeout: Cell<Seconds>,
    pub(super) error: Cell<Option<io::Error>>,
    pub(super) read_task: LocalWaker,
    pub(super) write_task: LocalWaker,
    pub(super) dispatch_task: LocalWaker,
    pub(super) read_buf: Cell<Option<BytesMut>>,
    pub(super) write_buf: Cell<Option<BytesMut>>,
    pub(super) filter: Cell<&'static dyn Filter>,
    on_disconnect: RefCell<Slab<Option<LocalWaker>>>,
}

impl IoStateInner {
    pub(super) fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    pub(super) fn remove_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.remove(f);
        self.flags.set(flags);
    }

    pub(super) fn notify_disconnect(&self) {
        let mut slab = self.on_disconnect.borrow_mut();
        for item in slab.iter_mut() {
            if let Some(waker) = item.1 {
                waker.wake();
            } else {
                *item.1 = Some(LocalWaker::default())
            }
        }
    }
}

impl Eq for IoStateInner {}

impl PartialEq for IoStateInner {
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self, other)
    }
}

impl hash::Hash for IoStateInner {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        (self as *const _ as usize).hash(state);
    }
}

impl Drop for IoStateInner {
    fn drop(&mut self) {
        if let Some(buf) = self.read_buf.take() {
            self.pool.get().release_read_buf(buf);
        }
        if let Some(buf) = self.write_buf.take() {
            self.pool.get().release_write_buf(buf);
        }
    }
}

impl IoState {
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
            disconnect_timeout: Cell::new(Seconds(1)),
            dispatch_task: LocalWaker::new(),
            read_task: LocalWaker::new(),
            write_task: LocalWaker::new(),
            read_buf: Cell::new(None),
            write_buf: Cell::new(None),
            filter: Cell::new(NullFilter::get()),
            on_disconnect: RefCell::new(Slab::new()),
        });

        let filter = Box::new(DefaultFilter::new(inner.clone()));
        let filter_ref: &'static dyn Filter = unsafe {
            let filter: &dyn Filter = filter.as_ref();
            std::mem::transmute(filter)
        };
        inner.filter.replace(filter_ref);

        // start io tasks
        io.start(ReadState(inner.clone()), WriteState(inner.clone()));

        IoState(inner, Box::into_raw(filter))
    }
}

impl<F> IoState<F> {
    #[inline]
    /// Set memory pool
    pub fn set_memory_pool(&self, pool: PoolRef) {
        if let Some(mut buf) = self.0.read_buf.take() {
            pool.move_in(&mut buf);
            self.0.read_buf.set(Some(buf));
        }
        if let Some(mut buf) = self.0.write_buf.take() {
            pool.move_in(&mut buf);
            self.0.write_buf.set(Some(buf));
        }
        self.0.pool.set(pool);
    }

    #[inline]
    /// Set io disconnect timeout in secs
    pub fn set_disconnect_timeout(&self, timeout: Seconds) {
        self.0.disconnect_timeout.set(timeout);
    }
}

impl<F> IoState<F> {
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
        self.0.flags.get().contains(Flags::IO_ERR)
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
    /// Stop io tasks
    ///
    /// Wake dispatcher when io tasks are stopped.
    pub fn stop(&self, cx: &mut Context<'_>) {
        let flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_STOP) {
            self.0.insert_flags(Flags::IO_STOP);
            self.0.read_task.wake();
            self.0.write_task.wake();
            self.0.dispatch_task.register(cx.waker());
        }
    }

    #[inline]
    /// Gracefully shutdown read and write io tasks
    pub fn shutdown(&self, cx: &mut Context<'_>) {
        let flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN | Flags::IO_STOP) {
            log::trace!("initiate io shutdown {:?}", flags);
            self.0.insert_flags(Flags::IO_SHUTDOWN);
            self.0.read_task.wake();
            self.0.write_task.wake();
            self.0.dispatch_task.register(cx.waker());
        }
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
    /// Register dispatcher task
    pub fn register_dispatcher(&self, cx: &mut Context<'_>) {
        self.0.dispatch_task.register(cx.waker());
    }

    #[inline]
    /// Mark dispatcher as stopped
    pub fn dispatcher_stopped(&self) {
        self.0.insert_flags(Flags::DSP_STOP);
    }

    #[inline]
    /// Get api for read task
    pub fn read(&'_ self) -> Read<'_> {
        Read(self.0.as_ref())
    }

    #[inline]
    /// Get api for write task
    pub fn write(&'_ self) -> Write<'_> {
        Write(self.0.as_ref())
    }

    #[inline]
    /// Get write handle, which gives access to write api
    pub fn write_handle(&self) -> WriteHandle {
        WriteHandle(self.0.clone())
    }

    #[inline]
    /// Gracefully close connection
    ///
    /// First stop dispatcher, then dispatcher stops io tasks
    fn _close(&self) {
        self.0.insert_flags(Flags::DSP_STOP);
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Force close connection
    ///
    /// Dispatcher does not wait for uncompleted responses, but flushes io buffers.
    fn force_close(&self) {
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

impl<F> IoState<F> {
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
            let mut buf = self.0.read_buf.take();
            let item = if let Some(ref mut buf) = buf {
                codec.decode(buf)
            } else {
                Ok(None)
            };
            self.0.read_buf.set(buf);

            let result = match item {
                Ok(Some(el)) => Ok(Some(el)),
                Ok(None) => {
                    poll_fn(|cx| {
                        if read.is_ready() {
                            Poll::Ready(())
                        } else {
                            read.wake(cx);
                            Poll::Pending
                        }
                    })
                    .await;
                    if self.is_io_err() {
                        if let Some(err) = self.take_error() {
                            Err(Either::Right(err))
                        } else {
                            Ok(None)
                        }
                    } else {
                        continue;
                    }
                }
                Err(err) => Err(Either::Left(err)),
            };
            return result;
        }
    }

    #[inline]
    /// Encode item, send to a peer
    pub async fn send<U>(
        &self,
        codec: &U,
        item: U::Item,
    ) -> Result<(), Either<U::Error, io::Error>>
    where
        U: Encoder,
    {
        let filter = self.0.filter.get();
        let mut buf = filter
            .get_write_buf()
            .unwrap_or_else(|| self.0.pool.get().get_write_buf());
        let is_write_sleep = buf.is_empty();
        codec.encode(item, &mut buf).map_err(Either::Left)?;
        filter.release_write_buf(buf);
        self.0.insert_flags(Flags::WR_WAIT);
        if is_write_sleep {
            self.0.write_task.wake();
        }

        poll_fn(|cx| {
            if !self.0.flags.get().contains(Flags::WR_WAIT) || self.is_io_err() {
                Poll::Ready(())
            } else {
                self.register_dispatcher(cx);
                Poll::Pending
            }
        })
        .await;

        if self.is_io_err() {
            let err = self.0.error.take().unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "Internal error")
            });
            Err(Either::Right(err))
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn poll_next<U>(
        &self,
        codec: &U,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<U::Item>, Either<U::Error, io::Error>>>
    where
        U: Decoder,
    {
        let mut buf = self.0.read_buf.take();
        let item = if let Some(ref mut buf) = buf {
            codec.decode(buf)
        } else {
            Ok(None)
        };
        self.0.read_buf.set(buf);

        match item {
            Ok(Some(el)) => Poll::Ready(Ok(Some(el))),
            Ok(None) => {
                self.read().wake(cx);
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(Either::Left(err))),
        }
    }
}

impl<F: Filter> IoState<F> {
    pub async fn add_filter<T>(self, factory: &T) -> Result<IoState<T::Filter>, T::Error>
    where
        T: FilterFactory<F>,
    {
        factory.create(self).await
    }

    pub fn map_filter<T, U>(mut self, map: T) -> IoState<U>
    where
        T: FnOnce(F) -> U,
        U: Filter,
    {
        assert!(!self.1.is_null());

        // get current filter
        let filter = unsafe {
            let filter = Box::new(map(*Box::from_raw(mem::replace(
                &mut self.1,
                std::ptr::null_mut(),
            ))));

            let filter_ref: &'static dyn Filter = {
                let filter: &dyn Filter = filter.as_ref();
                std::mem::transmute(filter)
            };
            self.0.filter.replace(filter_ref);
            filter
        };

        IoState(self.0.clone(), Box::into_raw(filter))
    }
}

impl<F> Drop for IoState<F> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            self.force_close();
            self.0.filter.set(NullFilter::get());
            unsafe { Box::from_raw(mem::replace(&mut self.1, std::ptr::null_mut())) };
        }
    }
}

#[derive(Clone)]
pub struct WriteHandle(Rc<IoStateInner>);

impl WriteHandle {
    #[inline]
    /// Get api for write task
    pub fn get(&'_ self) -> Write<'_> {
        Write(self.0.as_ref())
    }
}

#[derive(Copy, Clone)]
pub struct Write<'a>(pub(super) &'a IoStateInner);

impl<'a> Write<'a> {
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

    /// Get mut access to write buffer
    pub fn with_buf<F, R>(&self, f: F) -> R
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
        filter.release_write_buf(buf);
        result
    }

    #[inline]
    /// Write item to a buffer and wake up write task
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
            let result = codec.encode(item, &mut buf).map(|_| {
                if is_write_sleep {
                    self.0.write_task.wake();
                }
                buf.len() < hw
            });
            filter.release_write_buf(buf);
            result
        } else {
            Ok(true)
        }
    }

    #[inline]
    /// Write item to a buf and wake up io task
    pub fn encode_result<U, E>(
        &self,
        item: Result<Option<U::Item>, E>,
        codec: &U,
    ) -> Result<bool, Either<E, U::Error>>
    where
        U: Encoder,
    {
        let flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_ERR | Flags::DSP_ERR) {
            match item {
                Ok(Some(item)) => {
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

                    // encode item
                    if let Err(err) = codec.encode(item, &mut buf) {
                        log::trace!("Encoder error: {:?}", err);
                        filter.release_write_buf(buf);
                        self.0.insert_flags(Flags::DSP_STOP | Flags::DSP_ERR);
                        self.0.dispatch_task.wake();
                        return Err(Either::Right(err));
                    } else if is_write_sleep {
                        self.0.write_task.wake();
                    }
                    let result = Ok(buf.len() < hw);
                    filter.release_write_buf(buf);
                    result
                }
                Err(err) => {
                    self.0.insert_flags(Flags::DSP_STOP | Flags::DSP_ERR);
                    self.0.dispatch_task.wake();
                    Err(Either::Left(err))
                }
                _ => Ok(true),
            }
        } else {
            Ok(true)
        }
    }
}

#[derive(Copy, Clone)]
pub struct Read<'a>(&'a IoStateInner);

impl<'a> Read<'a> {
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
    /// Wake read task and instruct to read more data
    ///
    /// Only wakes if back-pressure is enabled on read task
    /// otherwise read is already awake.
    pub fn wake(&self, cx: &mut Context<'_>) {
        let mut flags = self.0.flags.get();
        flags.remove(Flags::RD_READY);
        if flags.contains(Flags::RD_BUF_FULL) {
            log::trace!("read back-pressure is enabled, wake io task");
            flags.remove(Flags::RD_BUF_FULL);
            self.0.read_task.wake();
        }
        if flags.contains(Flags::RD_PAUSED) {
            log::trace!("read is paused, wake io task");
            flags.remove(Flags::RD_PAUSED);
            self.0.read_task.wake();
        }
        self.0.flags.set(flags);
        self.0.dispatch_task.register(cx.waker());
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
            codec.decode(buf)
        } else {
            Ok(None)
        };
        self.0.read_buf.set(buf);
        result
    }

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
            self.0.pool.get().release_read_buf(buf);
        } else {
            self.0.read_buf.set(Some(buf));
        }
        res
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
        let token = inner.on_disconnect.borrow_mut().insert(if disconnected {
            Some(LocalWaker::default())
        } else {
            None
        });
        Self { token, inner }
    }

    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut on_disconnect = self.inner.on_disconnect.borrow_mut();

        let inner = unsafe { on_disconnect.get_unchecked_mut(self.token) };
        if inner.is_none() {
            let waker = LocalWaker::default();
            waker.register(cx.waker());
            *inner = Some(waker);
        } else if !inner.as_mut().unwrap().register(cx.waker()) {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Clone for OnDisconnect {
    fn clone(&self) -> Self {
        let token = self.inner.on_disconnect.borrow_mut().insert(None);
        OnDisconnect {
            token,
            inner: self.inner.clone(),
        }
    }
}

impl Future for OnDisconnect {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_ready(cx)
    }
}

impl Drop for OnDisconnect {
    fn drop(&mut self) {
        self.inner.on_disconnect.borrow_mut().remove(self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{Filter, FilterFactory, ReadFilter, WriteFilter, WriteReadiness};
    use crate::{codec::BytesCodec, testing::Io, util::lazy, util::Bytes, util::Ready};

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[crate::rt_test]
    async fn test_utils() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write(TEXT);

        let state = IoState::new(server);
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

        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        let state = IoState::new(server);

        client.read_error(io::Error::new(io::ErrorKind::Other, "err"));
        let res = poll_fn(|cx| Poll::Ready(state.poll_next(&BytesCodec, cx))).await;
        if let Poll::Ready(msg) = res {
            assert!(msg.is_err());
            assert!(state.flags().contains(Flags::IO_ERR));
            assert!(state.flags().contains(Flags::DSP_STOP));
        }

        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        let state = IoState::new(server);
        state
            .send(&BytesCodec, Bytes::from_static(b"test"))
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write_error(io::Error::new(io::ErrorKind::Other, "err"));
        let res = state.send(&BytesCodec, Bytes::from_static(b"test")).await;
        assert!(res.is_err());
        assert!(state.flags().contains(Flags::IO_ERR));
        assert!(state.flags().contains(Flags::DSP_STOP));

        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        let state = IoState::new(server);
        state.force_close();
        assert!(state.flags().contains(Flags::DSP_STOP));
        assert!(state.flags().contains(Flags::IO_SHUTDOWN));
    }

    #[crate::rt_test]
    async fn test_on_disconnect() {
        let (client, server) = Io::create();
        let state = IoState::new(server);
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

        let (client, server) = Io::create();
        let state = IoState::new(server);
        let mut waiter = state.on_disconnect();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Pending
        );
        client.read_error(io::Error::new(io::ErrorKind::Other, "err"));
        assert_eq!(waiter.await, ());
    }

    #[crate::rt_test]
    async fn filter() {
        struct Counter<F> {
            inner: F,
            in_bytes: Rc<Cell<usize>>,
            out_bytes: Rc<Cell<usize>>,
        }
        impl<F: ReadFilter + WriteFilter> Filter for Counter<F> {}

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

            fn release_read_buf(&self, buf: BytesMut, new_bytes: usize) {
                self.in_bytes.set(self.in_bytes.get() + new_bytes);
                self.inner.release_read_buf(buf, new_bytes);
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

            fn release_write_buf(&self, buf: BytesMut) {
                self.out_bytes.set(self.out_bytes.get() + buf.len());
                self.inner.release_write_buf(buf);
            }
        }

        struct CounterFactory(Rc<Cell<usize>>, Rc<Cell<usize>>);

        impl<F: Filter> FilterFactory<F> for CounterFactory {
            type Filter = Counter<F>;

            type Error = ();
            type Future = Ready<IoState<Counter<F>>, Self::Error>;

            fn create(&self, st: IoState<F>) -> Self::Future {
                let in_bytes = self.0.clone();
                let out_bytes = self.1.clone();
                Ready::Ok(st.map_filter(|inner| Counter {
                    inner,
                    in_bytes,
                    out_bytes,
                }))
            }
        }

        let in_bytes = Rc::new(Cell::new(0));
        let out_bytes = Rc::new(Cell::new(0));
        let factory = CounterFactory(in_bytes.clone(), out_bytes.clone());

        let (client, server) = Io::create();
        let state = IoState::new(server).add_filter(&factory).await.unwrap();

        client.remote_buffer_cap(1024);
        client.write(TEXT);
        let msg = state.next(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        state
            .send(&BytesCodec, Bytes::from_static(b"test"))
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        assert_eq!(in_bytes.get(), BIN.len());
        assert_eq!(out_bytes.get(), 4);
    }
}
