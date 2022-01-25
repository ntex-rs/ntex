use std::{any, fmt, hash, io};

use ntex_bytes::{BufMut, BytesMut, PoolRef};
use ntex_codec::{Decoder, Encoder};

use super::io::{Flags, IoRef, OnDisconnect};
use super::{types, Filter};

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
    /// Check if io stream is closed
    pub fn is_closed(&self) -> bool {
        self.0.flags.get().contains(Flags::IO_STOPPING)
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
        self.0.init_shutdown(None);
    }

    #[inline]
    /// Force close connection
    ///
    /// Dispatcher does not wait for uncompleted responses. Io stream get terminated
    /// without any graceful period.
    pub fn force_close(&self) {
        log::trace!("force close io stream object");
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
    pub fn want_shutdown(&self, err: Option<io::Error>) {
        self.0.init_shutdown(err);
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
        let flags = self.0.flags.get();

        if !flags.contains(Flags::IO_STOPPING) {
            self.with_write_buf(|buf| {
                let (hw, lw) = self.memory_pool().write_params().unpack();

                // make sure we've got room
                let remaining = buf.remaining_mut();
                if remaining < lw {
                    buf.reserve(hw - remaining);
                }

                // encode item and wake write task
                codec.encode(item, buf)
            })
            .map_or_else(
                |err| {
                    self.0.io_stopped(Some(err));
                    Ok(())
                },
                |item| item,
            )
        } else {
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
        self.0.with_read_buf(false, |buf| {
            buf.as_mut().map(|b| codec.decode(b)).unwrap_or(Ok(None))
        })
    }

    #[inline]
    /// Write bytes to a buffer and wake up write task
    pub fn write(&self, src: &[u8]) -> io::Result<()> {
        let flags = self.0.flags.get();

        if !flags.intersects(Flags::IO_STOPPING) {
            self.with_write_buf(|buf| {
                buf.extend_from_slice(src);
            })
        } else {
            Ok(())
        }
    }

    #[inline]
    /// Get mut access to write buffer
    pub fn with_write_buf<F, R>(&self, f: F) -> Result<R, io::Error>
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let filter = self.0.filter.get();
        let mut buf = filter
            .get_write_buf()
            .unwrap_or_else(|| self.memory_pool().get_write_buf());
        let is_write_sleep = buf.is_empty();

        let result = f(&mut buf);
        if is_write_sleep {
            self.0.write_task.wake();
        }
        filter.release_write_buf(buf)?;
        Ok(result)
    }

    #[inline]
    /// Get mut access to read buffer
    pub fn with_read_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.0.with_read_buf(true, |buf| {
            // set buf
            if buf.is_none() {
                *buf = Some(self.memory_pool().get_read_buf());
            }
            f(buf.as_mut().unwrap())
        })
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
            .field("open", &!self.is_closed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::{future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;
    use ntex_util::future::{lazy, poll_fn, Ready};
    use ntex_util::time::{sleep, Millis};

    use super::*;
    use crate::testing::IoTest;
    use crate::{Filter, FilterFactory, Io, ReadStatus, WriteStatus};

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[ntex::test]
    async fn utils() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write(TEXT);

        let state = Io::new(server);
        let msg = state.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

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
        state
            .send(Bytes::from_static(b"test"), &BytesCodec)
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
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

        let item = io.with_read_buf(|buffer| buffer.clone());
        assert_eq!(item, Bytes::from_static(BIN));

        client.write(TEXT);
        sleep(Millis(50)).await;
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_ready());
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());
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
        idx: usize,
        inner: F,
        in_bytes: Rc<Cell<usize>>,
        out_bytes: Rc<Cell<usize>>,
        read_order: Rc<RefCell<Vec<usize>>>,
        write_order: Rc<RefCell<Vec<usize>>>,
    }
    impl<F: Filter> Filter for Counter<F> {
        fn poll_shutdown(&self) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn query(&self, _: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
            None
        }

        fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
            self.inner.poll_read_ready(cx)
        }

        fn get_read_buf(&self) -> Option<BytesMut> {
            self.inner.get_read_buf()
        }

        fn release_read_buf(&self, buf: BytesMut) {
            self.inner.release_read_buf(buf)
        }

        fn process_read_buf(&self, io: &IoRef, n: usize) -> io::Result<(usize, usize)> {
            let result = self.inner.process_read_buf(io, n)?;
            self.read_order.borrow_mut().push(self.idx);
            self.in_bytes.set(self.in_bytes.get() + result.1);
            Ok(result)
        }

        fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
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
            self.write_order.borrow_mut().push(self.idx);
            self.out_bytes.set(self.out_bytes.get() + buf.len());
            self.inner.release_write_buf(buf)
        }
    }

    struct CounterFactory(
        usize,
        Rc<Cell<usize>>,
        Rc<Cell<usize>>,
        Rc<RefCell<Vec<usize>>>,
        Rc<RefCell<Vec<usize>>>,
    );

    impl<F: Filter> FilterFactory<F> for CounterFactory {
        type Filter = Counter<F>;

        type Error = ();
        type Future = Ready<Io<Counter<F>>, Self::Error>;

        fn create(self, io: Io<F>) -> Self::Future {
            let idx = self.0;
            let in_bytes = self.1.clone();
            let out_bytes = self.2.clone();
            let read_order = self.3.clone();
            let write_order = self.4;
            Ready::Ok(
                io.map_filter(|inner| {
                    Ok::<_, ()>(Counter {
                        idx,
                        inner,
                        in_bytes,
                        out_bytes,
                        read_order,
                        write_order,
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
        let read_order = Rc::new(RefCell::new(Vec::new()));
        let write_order = Rc::new(RefCell::new(Vec::new()));
        let factory = CounterFactory(
            1,
            in_bytes.clone(),
            out_bytes.clone(),
            read_order.clone(),
            write_order.clone(),
        );

        let (client, server) = IoTest::create();
        let state = Io::new(server).add_filter(factory).await.unwrap();

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
            .add_filter(CounterFactory(
                1,
                in_bytes.clone(),
                out_bytes.clone(),
                read_order.clone(),
                write_order.clone(),
            ))
            .await
            .unwrap()
            .add_filter(CounterFactory(
                2,
                in_bytes.clone(),
                out_bytes.clone(),
                read_order.clone(),
                write_order.clone(),
            ))
            .await
            .unwrap();
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
