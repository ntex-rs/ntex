use std::{any, fmt, hash, io, ptr};

use ntex_bytes::{BytePage, BytePages, BytesMut};
use ntex_codec::{Decoder, Encoder};
use ntex_service::cfg::SharedCfg;
use ntex_util::time::Seconds;

use crate::{
    Decoded, Filter, FilterBuf, Flags, IoConfig, IoContext, IoRef, OnDisconnect, timer,
    types,
};

impl IoRef {
    #[inline]
    /// Get tag
    pub fn tag(&self) -> &'static str {
        self.0.cfg.tag()
    }

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
    /// Get configuration
    pub fn cfg(&self) -> &IoConfig {
        &self.0.cfg
    }

    #[inline]
    /// Get shared configuration
    pub fn shared(&self) -> SharedCfg {
        self.0.cfg.shared()
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

    /// Gracefully close connection
    ///
    /// Initiate io stream shutdown process.
    pub fn close(&self) {
        self.0.init_shutdown();
    }

    /// Force close connection
    ///
    /// Dispatcher does not wait for uncompleted responses. Io stream get terminated
    /// without any graceful period.
    pub fn force_close(&self) {
        log::trace!("{}: Force close io stream object", self.tag());
        self.0.insert_flags(
            Flags::IO_STOPPED | Flags::IO_STOPPING | Flags::IO_STOPPING_FILTERS,
        );
        self.0.read_task.wake();
        self.0.write_task.wake();
        self.0.dispatch_task.wake();
    }

    /// Ask io to process write buffer
    pub fn wants_write(&self) {
        self.0.insert_flags(Flags::IO_WANT_WRITE);
    }

    /// Gracefully shutdown io stream
    pub fn wants_shutdown(&self) {
        if !self.0.flags.get().is_stopping() {
            log::trace!(
                "{}: Initiate io shutdown {:?}",
                self.tag(),
                self.0.flags.get()
            );
            self.0.insert_flags(Flags::IO_STOPPING_FILTERS);
            self.0.read_task.wake();
        }
    }

    /// Query filter specific data
    pub fn query<T: 'static>(&self) -> types::QueryItem<T> {
        if let Some(item) = self.filter().query(any::TypeId::of::<T>()) {
            types::QueryItem::new(item)
        } else {
            types::QueryItem::empty()
        }
    }

    /// Encode and write item to a buffer and wake up write task
    pub fn encode<U>(&self, item: U::Item, codec: &U) -> Result<(), <U as Encoder>::Error>
    where
        U: Encoder,
    {
        if self.is_closed() {
            log::trace!("{}: Io is closed/closing, skip frame encoding", self.tag());
            Ok(())
        } else {
            // encode item and wake write task
            self.with_write_buf(|buf| codec.encodev(item, buf))
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
        }
    }

    /// Encode slice to a buffer and wake up write task
    pub fn encode_slice(&self, src: &[u8]) -> io::Result<()> {
        self.with_write_buf(|buf| buf.extend_from_slice(src))
    }

    /// Write bytes to a buffer and wake up write task
    pub fn encode_bytes<B>(&self, src: B) -> io::Result<()>
    where
        BytePage: From<B>,
    {
        self.with_write_buf(|buf| buf.append(src))
    }

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
            .with_read_destination(self, |buf| codec.decode(buf))
    }

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
            codec.decode(buf).map(|item| Decoded {
                item,
                remains: buf.len(),
                consumed: len - buf.len(),
            })
        })
    }

    /// Get access to filter buffer
    pub fn with_buf<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut FilterBuf<'_>) -> R,
    {
        self.0.buffer.with_filter(self, |ctx| {
            let result = ctx.with_buffer(f);
            self.0.filter().process_write_buf(ctx)?;
            Ok(result)
        })
    }

    /// Get mut access to read buffer
    pub fn with_read_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.0.buffer.with_read_destination(self, f)
    }

    /// Get mut access to source write buffer
    pub fn with_write_buf<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        if self.0.flags.get().contains(Flags::IO_STOPPED) {
            Err(self.0.error_or_disconnected())
        } else {
            let (wrt, res) = self.0.buffer.with_write_source(|pages| {
                let result = f(pages);

                // wake write task if needsed
                let mut wrt = false;
                let len = pages.len();
                let flags = self.0.flags.get();
                if len > 0 && flags.write_paused() {
                    // The app encodes data in response to incoming data,
                    // continuing to fill the write buffer until all data
                    // has been processed. Only then can the runtime wake
                    // the write task to send the buffered data.
                    //
                    // By that time, the buffer may have accumulated a large
                    // amount of data, causing it to be sent in large bursts,
                    // which introduces latency. To prevent this behavior and
                    // flatten data delivery to the peer, IoRef can initiate
                    // out-of-order writes based on a configured threshold.
                    if flags.can_write_upfront() && len > self.0.cfg.write_buf_limit() {
                        wrt = true;
                    } else {
                        self.0.write_task.wake();
                    }
                    self.0.remove_flags(Flags::WR_PAUSED);
                }
                // enable backpressure
                if len >= self.0.cfg.write_buf().high
                    && !flags.contains(Flags::BUF_W_BACKPRESSURE)
                {
                    self.0.insert_flags(Flags::BUF_W_BACKPRESSURE);
                    self.0.dispatch_task.wake();
                }

                (wrt, Ok(result))
            });

            if wrt && let Some(hnd) = self.0.handle.take() {
                let ctx = unsafe { &*(ptr::from_ref(self).cast::<IoContext>()) };
                hnd.write(ctx);
                self.0.handle.set(Some(hnd));
                if !self.0.flags.get().write_paused() {
                    self.0.write_task.wake();
                }
            }
            res
        }
    }

    /// Make sure buffer has enough free space
    pub fn resize_read_buf(&self, buf: &mut BytesMut) {
        self.0.cfg.read_buf().resize(buf);
    }

    /// Wakeup dispatcher
    pub fn notify_dispatcher(&self) {
        self.0.dispatch_task.wake();
        log::trace!("{}: Timer, notify dispatcher", self.tag());
    }

    /// Wakeup dispatcher and send keep-alive error
    pub fn notify_timeout(&self) {
        self.0.notify_timeout();
    }

    /// current timer handle
    pub fn timer_handle(&self) -> timer::TimerHandle {
        self.0.timeout.get()
    }

    /// Start timer
    pub fn start_timer(&self, timeout: Seconds) -> timer::TimerHandle {
        let cur_hnd = self.0.timeout.get();

        if timeout.is_zero() {
            if cur_hnd.is_set() {
                self.0.timeout.set(timer::TimerHandle::ZERO);
                timer::unregister(cur_hnd, self);
            }
            timer::TimerHandle::ZERO
        } else if cur_hnd.is_set() {
            let hnd = timer::update(cur_hnd, timeout, self);
            if hnd != cur_hnd {
                log::trace!("{}: Update timer {:?}", self.tag(), timeout);
                self.0.timeout.set(hnd);
            }
            hnd
        } else {
            log::trace!("{}: Start timer {:?}", self.tag(), timeout);
            let hnd = timer::register(timeout, self);
            self.0.timeout.set(hnd);
            hnd
        }
    }

    /// Stop timer
    pub fn stop_timer(&self) {
        let hnd = self.0.timeout.get();
        if hnd.is_set() {
            log::trace!("{}: Stop timer", self.tag());
            self.0.timeout.set(timer::TimerHandle::ZERO);
            timer::unregister(hnd, self);
        }
    }

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
    use std::{future::Future, future::poll_fn, pin::Pin, rc::Rc, task::Poll};

    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;
    use ntex_util::{future::lazy, time::Millis, time::sleep};

    use super::*;
    use crate::{FilterCtx, Io, testing::IoTest};

    const BIN: &[u8] = b"GET /test HTTP/1\r\n\r\n";
    const TEXT: &str = "GET /test HTTP/1\r\n\r\n";

    #[ntex::test]
    async fn utils() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write(TEXT);

        let state = Io::from(server);
        assert_eq!(state.get_ref(), state.get_ref());

        let msg = state.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));
        assert_eq!(state.get_ref(), state.as_ref().clone());
        assert!(format!("{state:?}").find("Io {").is_some());
        assert!(format!("{:?}", state.get_ref()).find("IoRef {").is_some());

        let res = poll_fn(|cx| Poll::Ready(state.poll_recv(&BytesCodec, cx))).await;
        assert!(res.is_pending());
        client.write(TEXT);
        sleep(Millis(50)).await;
        let res = poll_fn(|cx| Poll::Ready(state.poll_recv(&BytesCodec, cx))).await;
        if let Poll::Ready(msg) = res {
            assert_eq!(msg.unwrap(), Bytes::from_static(BIN));
        }

        client.read_error(io::Error::other("err"));
        let msg = state.recv(&BytesCodec).await;
        assert!(msg.is_err());
        assert!(state.flags().contains(Flags::IO_STOPPED));

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::from(server);

        client.read_error(io::Error::other("err"));
        let res = poll_fn(|cx| Poll::Ready(state.poll_recv(&BytesCodec, cx))).await;
        if let Poll::Ready(msg) = res {
            assert!(msg.is_err());
            assert!(state.flags().contains(Flags::IO_STOPPED));
        }

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::from(server);
        state.encode_slice(b"test").unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write(b"test");
        state.read_ready().await.unwrap();
        let buf = state.decode(&BytesCodec).unwrap().unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write_error(io::Error::other("err"));
        let res = state.send(Bytes::from_static(b"test"), &BytesCodec).await;
        assert!(res.is_err());
        assert!(state.flags().contains(Flags::IO_STOPPED));

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::from(server);
        state.force_close();
        assert!(state.flags().contains(Flags::IO_STOPPED));
        assert!(state.flags().contains(Flags::IO_STOPPING));
    }

    #[ntex::test]
    async fn read_readiness() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let io = Io::from(server);
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());

        client.write(TEXT);
        assert_eq!(io.read_ready().await.unwrap(), Some(()));
        assert!(lazy(|cx| io.poll_read_ready(cx)).await.is_pending());

        let item = io.with_read_buf(BytesMut::take);
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
        let state = Io::from(server);
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
        let state = Io::from(server);
        let mut waiter = state.on_disconnect();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Pending
        );
        client.read_error(io::Error::other("err"));
        assert_eq!(waiter.await, ());
    }

    #[ntex::test]
    async fn write_to_closed_io() {
        let (client, server) = IoTest::create();
        let state = Io::from(server);
        client.close().await;

        assert!(state.is_closed());
        assert!(state.encode_slice(TEXT.as_bytes()).is_err());
        assert!(
            state
                .with_write_buf(|buf| buf.extend_from_slice(BIN))
                .is_err()
        );
    }

    #[derive(Debug)]
    struct Counter<F> {
        layer: F,
        idx: usize,
        in_bytes: Rc<Cell<usize>>,
        out_bytes: Rc<Cell<usize>>,
        read_order: Rc<RefCell<Vec<usize>>>,
        write_order: Rc<RefCell<Vec<usize>>>,
    }

    impl<F: Filter> Filter for Counter<F> {
        fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
            self.read_order.borrow_mut().push(self.idx);
            let nbytes =
                ctx.with_buffer(|b| b.read_dst().as_ref().map_or(0, BytesMut::len));
            let result = self.layer.process_read_buf(ctx);
            let nbytes2 =
                ctx.with_buffer(|b| b.read_dst().as_ref().map_or(0, BytesMut::len));
            self.in_bytes.set(self.in_bytes.get() + nbytes2 - nbytes);
            result
        }

        fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
            self.write_order.borrow_mut().push(self.idx);
            ctx.with_buffer(|buf| {
                buf.with_write_buffers(|_, src, _| {
                    self.out_bytes.set(self.out_bytes.get() + src.len());
                });
            });
            self.layer.process_write_buf(ctx)
        }

        crate::forward_ready!(layer);
        crate::forward_query!(layer);
        crate::forward_shutdown!(layer);
    }

    #[ntex::test]
    async fn filter() {
        let in_bytes = Rc::new(Cell::new(0));
        let out_bytes = Rc::new(Cell::new(0));
        let read_order = Rc::new(RefCell::new(Vec::new()));
        let write_order = Rc::new(RefCell::new(Vec::new()));

        let (client, server) = IoTest::create();
        let io = Io::from(server).map_filter(|layer| Counter {
            layer,
            idx: 1,
            in_bytes: in_bytes.clone(),
            out_bytes: out_bytes.clone(),
            read_order: read_order.clone(),
            write_order: write_order.clone(),
        });

        client.remote_buffer_cap(1024);
        client.write(TEXT);
        let msg = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        io.send(Bytes::from_static(b"test"), &BytesCodec)
            .await
            .unwrap();
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write(TEXT);
        let msg = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(msg, Bytes::from_static(BIN));

        assert_eq!(in_bytes.get(), BIN.len() * 2);
        assert_eq!(out_bytes.get(), 8);
    }

    #[ntex::test]
    async fn boxed_filter() {
        let in_bytes = Rc::new(Cell::new(0));
        let out_bytes = Rc::new(Cell::new(0));
        let read_order = Rc::new(RefCell::new(Vec::new()));
        let write_order = Rc::new(RefCell::new(Vec::new()));

        let (client, server) = IoTest::create();
        let state = Io::from(server)
            .map_filter(|layer| Counter {
                layer,
                idx: 2,
                in_bytes: in_bytes.clone(),
                out_bytes: out_bytes.clone(),
                read_order: read_order.clone(),
                write_order: write_order.clone(),
            })
            .map_filter(|layer| Counter {
                layer,
                idx: 1,
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
        assert_eq!(out_bytes.get(), 16);
        assert_eq!(state.0.buffer.with_write_destination(|b| b.len()), 0);

        // refs
        assert_eq!(Rc::strong_count(&in_bytes), 3);
        drop(state);
        assert_eq!(Rc::strong_count(&in_bytes), 1);
        assert_eq!(*read_order.borrow(), &[1, 2][..]);
        assert_eq!(*write_order.borrow(), &[1, 2, 1, 2, 1, 2][..]);
    }
}
