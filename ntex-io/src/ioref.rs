use std::{any, fmt, hash, io};

use ntex_bytes::{BytesVec, PoolRef};
use ntex_codec::{Decoder, Encoder};
use ntex_util::time::Seconds;

use crate::{timer, types, Decoded, Filter, Flags, IoRef, OnDisconnect, WriteBuf};

impl IoRef {
    #[inline]
    #[doc(hidden)]
    /// Get current state flags
    pub fn flags(&self) -> Flags {
        self.0.flags.get()
    }

    #[inline]
    /// Get current filter
    pub(crate) fn filter(&self) -> &dyn Filter {
        self.0.filter()
    }

    #[inline]
    /// Get memory pool
    pub fn memory_pool(&self) -> PoolRef {
        self.0.pool.get()
    }

    #[inline]
    /// Check if io stream is closed
    pub fn is_closed(&self) -> bool {
        self.0
            .flags
            .get()
            .intersects(Flags::IO_STOPPING | Flags::IO_STOPPED)
    }

    #[inline]
    /// Check if write back-pressure is enabled
    pub fn is_wr_backpressure(&self) -> bool {
        self.0.flags.get().contains(Flags::BUF_W_BACKPRESSURE)
    }

    #[inline]
    /// Wake dispatcher task
    pub fn wake(&self) {
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Gracefully close connection
    ///
    /// Notify dispatcher and initiate io stream shutdown process.
    pub fn close(&self) {
        self.0.insert_flags(Flags::DSP_STOP);
        self.0.init_shutdown();
    }

    #[inline]
    /// Force close connection
    ///
    /// Dispatcher does not wait for uncompleted responses. Io stream get terminated
    /// without any graceful period.
    pub fn force_close(&self) {
        log::trace!("{}: Force close io stream object", self.tag());
        self.0.insert_flags(
            Flags::DSP_STOP
                | Flags::IO_STOPPED
                | Flags::IO_STOPPING
                | Flags::IO_STOPPING_FILTERS,
        );
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
    }

    #[inline]
    /// Gracefully shutdown io stream
    pub fn want_shutdown(&self) {
        if !self
            .0
            .flags
            .get()
            .intersects(Flags::IO_STOPPED | Flags::IO_STOPPING | Flags::IO_STOPPING_FILTERS)
        {
            log::trace!(
                "{}: Initiate io shutdown {:?}",
                self.tag(),
                self.0.flags.get()
            );
            self.0.insert_flags(Flags::IO_STOPPING_FILTERS);
            self.0.read_task.wake();
        }
    }

    #[inline]
    /// Query filter specific data
    pub fn query<T: 'static>(&self) -> types::QueryItem<T> {
        if let Some(item) = self.filter().query(any::TypeId::of::<T>()) {
            types::QueryItem::new(item)
        } else {
            types::QueryItem::empty()
        }
    }

    #[inline]
    /// Encode and write item to a buffer and wake up write task
    pub fn encode<U>(&self, item: U::Item, codec: &U) -> Result<(), <U as Encoder>::Error>
    where
        U: Encoder,
    {
        if !self.is_closed() {
            self.with_write_buf(|buf| {
                // make sure we've got room
                self.memory_pool().resize_write_buf(buf);

                // encode item and wake write task
                codec.encode_vec(item, buf)
            })
            // .with_write_buf() could return io::Error<Result<(), U::Error>>,
            // in that case mark io as failed
            .unwrap_or_else(|err| {
                log::trace!(
                    "{}: Got io error while encoding, error: {:?}",
                    self.tag(),
                    err
                );
                self.0.io_stopped(Some(err));
                Ok(())
            })
        } else {
            log::trace!("{}: Io is closed/closing, skip frame encoding", self.tag());
            Ok(())
        }
    }

    #[inline]
    /// Attempts to decode a frame from the read buffer
    pub fn decode<U>(
        &self,
        codec: &U,
    ) -> Result<Option<<U as Decoder>::Item>, <U as Decoder>::Error>
    where
        U: Decoder,
    {
        self.0
            .buffer
            .with_read_destination(self, |buf| codec.decode_vec(buf))
    }

    #[inline]
    /// Attempts to decode a frame from the read buffer
    pub fn decode_item<U>(
        &self,
        codec: &U,
    ) -> Result<Decoded<<U as Decoder>::Item>, <U as Decoder>::Error>
    where
        U: Decoder,
    {
        self.0.buffer.with_read_destination(self, |buf| {
            let len = buf.len();
            codec.decode_vec(buf).map(|item| Decoded {
                item,
                remains: buf.len(),
                consumed: len - buf.len(),
            })
        })
    }

    #[inline]
    /// Write bytes to a buffer and wake up write task
    pub fn write(&self, src: &[u8]) -> io::Result<()> {
        self.with_write_buf(|buf| buf.extend_from_slice(src))
    }

    #[inline]
    /// Get access to write buffer
    pub fn with_buf<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&WriteBuf<'_>) -> R,
    {
        let result = self.0.buffer.write_buf(self, 0, f);
        self.0.filter().process_write_buf(self, &self.0.buffer, 0)?;
        Ok(result)
    }

    #[inline]
    /// Get mut access to source write buffer
    pub fn with_write_buf<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        if self.0.flags.get().contains(Flags::IO_STOPPED) {
            Err(io::Error::new(io::ErrorKind::Other, "Disconnected"))
        } else {
            let result = self.0.buffer.with_write_source(self, f);
            self.0.filter().process_write_buf(self, &self.0.buffer, 0)?;
            Ok(result)
        }
    }

    #[inline]
    /// Get mut access to source read buffer
    pub fn with_read_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        self.0.buffer.with_read_destination(self, f)
    }

    #[inline]
    /// Wakeup dispatcher
    pub fn notify_dispatcher(&self) {
        self.0.dispatch_task.wake();
        log::trace!("{}: Timer, notify dispatcher", self.tag());
    }

    #[inline]
    /// Wakeup dispatcher and send keep-alive error
    pub fn notify_timeout(&self) {
        self.0.notify_timeout()
    }

    #[inline]
    /// current timer handle
    pub fn timer_handle(&self) -> timer::TimerHandle {
        self.0.timeout.get()
    }

    #[inline]
    /// Start timer
    pub fn start_timer(&self, timeout: Seconds) -> timer::TimerHandle {
        let cur_hnd = self.0.timeout.get();

        if !timeout.is_zero() {
            if cur_hnd.is_set() {
                let hnd = timer::update(cur_hnd, timeout, self);
                if hnd != cur_hnd {
                    log::debug!("{}: Update timer {:?}", self.tag(), timeout);
                    self.0.timeout.set(hnd);
                }
                hnd
            } else {
                log::debug!("{}: Start timer {:?}", self.tag(), timeout);
                let hnd = timer::register(timeout, self);
                self.0.timeout.set(hnd);
                hnd
            }
        } else {
            if cur_hnd.is_set() {
                self.0.timeout.set(timer::TimerHandle::ZERO);
                timer::unregister(cur_hnd, self);
            }
            timer::TimerHandle::ZERO
        }
    }

    #[inline]
    /// Stop timer
    pub fn stop_timer(&self) {
        let hnd = self.0.timeout.get();
        if hnd.is_set() {
            log::debug!("{}: Stop timer", self.tag());
            self.0.timeout.set(timer::TimerHandle::ZERO);
            timer::unregister(hnd, self)
        }
    }

    #[inline]
    /// Get tag
    pub fn tag(&self) -> &'static str {
        self.0.tag.get()
    }

    #[inline]
    /// Set tag
    pub fn set_tag(&self, tag: &'static str) {
        self.0.tag.set(tag)
    }

    #[inline]
    /// Notify when io stream get disconnected
    pub fn on_disconnect(&self) -> OnDisconnect {
        OnDisconnect::new(self.0.clone())
    }
}

impl Eq for IoRef {}

impl PartialEq for IoRef {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

impl hash::Hash for IoRef {
    #[inline]
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for IoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoRef")
            .field("state", self.0.as_ref())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::{future::poll_fn, future::Future, pin::Pin, rc::Rc, task::Poll};

    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;
    use ntex_util::future::lazy;
    use ntex_util::time::{sleep, Millis};

    use super::*;
    use crate::{testing::IoTest, FilterLayer, Io, ReadBuf};

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[ntex::test]
    async fn utils() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write(TEXT);

        let state = Io::new(server);
        assert_eq!(state.get_ref(), state.get_ref());

        let msg = state.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));
        assert_eq!(state.get_ref(), state.as_ref().clone());
        assert!(format!("{:?}", state).find("Io {").is_some());
        assert!(format!("{:?}", state.get_ref()).find("IoRef {").is_some());

        let res = poll_fn(|cx| Poll::Ready(state.poll_recv(&BytesCodec, cx))).await;
        assert!(res.is_pending());
        client.write(TEXT);
        sleep(Millis(50)).await;
        let res = poll_fn(|cx| Poll::Ready(state.poll_recv(&BytesCodec, cx))).await;
        if let Poll::Ready(msg) = res {
            assert_eq!(msg.unwrap(), Bytes::from_static(BIN));
        }

        client.read_error(io::Error::new(io::ErrorKind::Other, "err"));
        let msg = state.recv(&BytesCodec).await;
        assert!(msg.is_err());
        assert!(state.flags().contains(Flags::IO_STOPPED));

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::new(server);

        client.read_error(io::Error::new(io::ErrorKind::Other, "err"));
        let res = poll_fn(|cx| Poll::Ready(state.poll_recv(&BytesCodec, cx))).await;
        if let Poll::Ready(msg) = res {
            assert!(msg.is_err());
            assert!(state.flags().contains(Flags::IO_STOPPED));
            assert!(state.flags().contains(Flags::DSP_STOP));
        }

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::new(server);
        state.write(b"test").unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write(b"test");
        state.read_ready().await.unwrap();
        let buf = state.decode(&BytesCodec).unwrap().unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write_error(io::Error::new(io::ErrorKind::Other, "err"));
        let res = state.send(Bytes::from_static(b"test"), &BytesCodec).await;
        assert!(res.is_err());
        assert!(state.flags().contains(Flags::IO_STOPPED));

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::new(server);
        state.force_close();
        assert!(state.flags().contains(Flags::DSP_STOP));
        assert!(state.flags().contains(Flags::IO_STOPPING));
    }

    #[ntex::test]
    async fn read_readiness() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let io = Io::new(server);
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());

        client.write(TEXT);
        assert_eq!(io.read_ready().await.unwrap(), Some(()));
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());

        let item = io.with_read_buf(|buffer| buffer.split());
        assert_eq!(item, Bytes::from_static(BIN));

        client.write(TEXT);
        sleep(Millis(50)).await;
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_ready());
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());
    }

    #[ntex::test]
    #[allow(clippy::unit_cmp)]
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

    #[ntex::test]
    async fn write_to_closed_io() {
        let (client, server) = IoTest::create();
        let state = Io::new(server);
        client.close().await;

        assert!(state.is_closed());
        assert!(state.write(TEXT.as_bytes()).is_err());
        assert!(state
            .with_write_buf(|buf| buf.extend_from_slice(BIN))
            .is_err());
    }

    #[derive(Debug)]
    struct Counter {
        idx: usize,
        in_bytes: Rc<Cell<usize>>,
        out_bytes: Rc<Cell<usize>>,
        read_order: Rc<RefCell<Vec<usize>>>,
        write_order: Rc<RefCell<Vec<usize>>>,
    }

    impl FilterLayer for Counter {
        const BUFFERS: bool = false;

        fn process_read_buf(&self, buf: &ReadBuf<'_>) -> io::Result<usize> {
            self.read_order.borrow_mut().push(self.idx);
            self.in_bytes.set(self.in_bytes.get() + buf.nbytes());
            Ok(buf.nbytes())
        }

        fn process_write_buf(&self, buf: &WriteBuf<'_>) -> io::Result<()> {
            self.write_order.borrow_mut().push(self.idx);
            self.out_bytes
                .set(self.out_bytes.get() + buf.with_dst(|b| b.len()));
            Ok(())
        }
    }

    #[ntex::test]
    async fn filter() {
        let in_bytes = Rc::new(Cell::new(0));
        let out_bytes = Rc::new(Cell::new(0));
        let read_order = Rc::new(RefCell::new(Vec::new()));
        let write_order = Rc::new(RefCell::new(Vec::new()));

        let (client, server) = IoTest::create();
        let counter = Counter {
            idx: 1,
            in_bytes: in_bytes.clone(),
            out_bytes: out_bytes.clone(),
            read_order: read_order.clone(),
            write_order: write_order.clone(),
        };
        let _ = format!("{:?}", counter);
        let io = Io::new(server).add_filter(counter);

        client.remote_buffer_cap(1024);
        client.write(TEXT);
        let msg = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        io.send(Bytes::from_static(b"test"), &BytesCodec)
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
        let read_order = Rc::new(RefCell::new(Vec::new()));
        let write_order = Rc::new(RefCell::new(Vec::new()));

        let (client, server) = IoTest::create();
        let state = Io::new(server)
            .add_filter(Counter {
                idx: 1,
                in_bytes: in_bytes.clone(),
                out_bytes: out_bytes.clone(),
                read_order: read_order.clone(),
                write_order: write_order.clone(),
            })
            .add_filter(Counter {
                idx: 2,
                in_bytes: in_bytes.clone(),
                out_bytes: out_bytes.clone(),
                read_order: read_order.clone(),
                write_order: write_order.clone(),
            });
        let state = state.seal();

        client.remote_buffer_cap(1024);
        client.write(TEXT);
        let msg = state.recv(&BytesCodec).await.unwrap().unwrap();
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
        assert_eq!(*read_order.borrow(), &[1, 2][..]);
        assert_eq!(*write_order.borrow(), &[2, 1][..]);
    }
}
