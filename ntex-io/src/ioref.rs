use std::{any, fmt, hash, io, ptr};

use ntex_bytes::{BytePage, BytePages, BytesMut};
use ntex_codec::{Decoder, Encoder};
use ntex_service::cfg::SharedCfg;
use ntex_util::time::Seconds;

use crate::ops::{Id, Iops, TimerHandle};
use crate::{Decoded, Filter, FilterBuf, Flags, IoConfig, IoContext, IoRef, types};

impl IoRef {
    #[inline]
    /// Gets the ID.
    pub fn id(&self) -> Id {
        self.0.id()
    }

    #[inline]
    /// Gets the tag.
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[doc(hidden)]
    /// Gets the current state flags.
    pub fn flags(&self) -> Flags {
        self.0.flags.clone()
    }

    #[inline]
    /// Gets the current filter.
    pub(crate) fn filter(&self) -> &dyn Filter {
        self.0.filter()
    }

    #[inline]
    /// Gets the configuration.
    pub fn cfg(&self) -> &IoConfig {
        &self.0.cfg
    }

    #[inline]
    /// Gets the shared configuration.
    pub fn shared(&self) -> SharedCfg {
        self.0.cfg.shared()
    }

    #[inline]
    /// Checks whether the I/O stream is closed.
    pub fn is_closed(&self) -> bool {
        self.0.flags.is_closed()
    }

    #[inline]
    /// Checks whether write back-pressure is enabled.
    pub fn is_wr_backpressure(&self) -> bool {
        self.0.flags.is_wr_backpressure()
    }

    /// Gracefully closes the connection.
    ///
    /// Initiates the I/O stream shutdown process.
    pub fn close(&self) {
        self.0.start_shutdown();
    }

    /// Force-closes the connection.
    ///
    /// The dispatcher does not wait for incomplete responses. The I/O stream is
    /// terminated without any graceful period.
    pub fn terminate(&self) {
        log::trace!("{}: Terminate io stream object", self.tag());
        self.0.terminate_connection(None);
    }

    #[doc(hidden)]
    #[deprecated(since = "3.10.0", note = "use Io::terminate() instead")]
    /// Force close connection
    ///
    /// Dispatcher does not wait for uncompleted responses. Io stream get terminated
    /// without any graceful period.
    pub fn force_close(&self) {
        self.terminate();
    }

    /// Gracefully shuts down the I/O stream.
    pub fn wants_shutdown(&self) {
        if !self.0.flags.is_stopping_any() {
            log::trace!("{}: Initiate io shutdown {:?}", self.tag(), self.0.flags);
            self.0.wake_read_task();
            self.0.wake_write_task();
            self.0.flags.set_filter_stopping();
        }
    }

    /// Queries filter-specific data.
    pub fn query<T: 'static>(&self) -> types::QueryItem<T> {
        if let Some(item) = self.filter().query(any::TypeId::of::<T>()) {
            types::QueryItem::new(item)
        } else {
            types::QueryItem::empty()
        }
    }

    #[inline]
    /// Encodes the item into the write buffer.
    pub fn encode<U>(&self, item: U::Item, codec: &U) -> Result<(), <U as Encoder>::Error>
    where
        U: Encoder,
    {
        self.with_write_buf(|buf| codec.encodev(item, buf))
            .unwrap_or_else(|_| Ok(()))
    }

    #[inline]
    /// Encodes the slice into the write buffer.
    pub fn encode_slice(&self, src: &[u8]) -> io::Result<()> {
        self.with_write_buf(|buf| buf.extend_from_slice(src))
    }

    #[inline]
    /// Writes bytes to the write buffer.
    pub fn encode_bytes<B>(&self, src: B) -> io::Result<()>
    where
        BytePage: From<B>,
    {
        self.with_write_buf(|buf| buf.append(src))
    }

    /// Attempts to decode a frame from the read buffer.
    pub fn decode<U>(
        &self,
        codec: &U,
    ) -> Result<Option<<U as Decoder>::Item>, <U as Decoder>::Error>
    where
        U: Decoder,
    {
        self.0.buffer.with_read_dst(self, |buf| {
            let res = codec.decode(buf);
            self.0.flags.unset_read_ready();
            self.update_read_destination(buf);
            res
        })
    }

    /// Attempts to decode a frame from the read buffer.
    pub fn decode_item<U>(
        &self,
        codec: &U,
    ) -> Result<Decoded<<U as Decoder>::Item>, <U as Decoder>::Error>
    where
        U: Decoder,
    {
        self.0.buffer.with_read_dst(self, |buf| {
            let len = buf.len();
            let res = codec.decode(buf).map(|item| Decoded {
                item,
                remains: buf.len(),
                consumed: len - buf.len(),
            });
            self.0.flags.unset_read_ready();
            self.update_read_destination(buf);
            res
        })
    }

    /// Sends the write buffer to the I/O layer.
    ///
    /// Requires the underlying runtime to implement `.write()`;
    /// otherwise, no action is taken.
    pub fn send_buf(&self) -> io::Result<()> {
        // can send if write task is not awake
        if self.0.flags.is_write_paused() {
            if self.call_write() == WakeWriteTask::Yes {
                // io write is pending need to wake write task for io completeion
                Iops::register_send(self.id());
            }

            if self.0.flags.is_stopping_any()
                && let Some(err) = self.0.error.take()
            {
                return Err(err);
            }
        }
        Ok(())
    }

    pub(crate) fn ops_send_buf(&self) {
        self.0.flags.unset_wr_send_scheduled();

        if self.0.flags.is_write_paused() {
            // call `Handle::write()`.
            // if write task is not paused, io write is pending
            // need to wake write task for io completeion
            if self.call_write() == WakeWriteTask::Yes {
                self.0.wake_write_task();
                self.0.flags.unset_write_paused();
            }
        }
    }

    /// Get access to filter buffer
    pub fn with_buf<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut FilterBuf<'_>) -> R,
    {
        let result = self.0.buffer.with_filter(self, |ctx| ctx.with_buffer(f));
        self.update_write_destination();
        Ok(result)
    }

    /// Get mut access to read buffer
    pub fn with_read_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.0.buffer.with_read_dst(self, |buf| {
            let res = f(buf);
            self.update_read_destination(buf);
            res
        })
    }

    /// Get mut access to source write buffer
    pub fn with_write_buf<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        let st = &self.0;

        if st.flags.is_terminated() {
            Err(st.error_or_disconnected())
        } else {
            let result = st.buffer.with_write_src(f);
            self.update_write_destination();
            Ok(result)
        }
    }

    pub(crate) fn update_write_destination(&self) {
        let st = &self.0;

        // wake write task if needsed
        let size = st.buffer.write_buf_size();

        if size > 0 && st.flags.is_write_paused() {
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
            if st.flags.is_send_buf_enabled() && size >= st.cfg.write_buf_threshold() {
                // Send data in-place
                if self.call_write() == WakeWriteTask::Yes {
                    // More data needs to be sent
                    st.flags.set_wr_send_scheduled();
                    Iops::register_send(st.id());
                }
            } else if !st.flags.is_wr_send_scheduled() {
                st.flags.set_wr_send_scheduled();
                Iops::register_send(st.id());
            }
        }
        // Enable backpressure
        if !st.flags.is_wr_backpressure() && st.is_wr_backpressure_needed(size) {
            st.flags.set_wr_backpressure();
            st.wake_dispatch_task();
        }
    }

    fn update_read_destination(&self, buf: &mut BytesMut) {
        let st = &self.0;
        let half = self.cfg().read_buf().half;

        if st.flags.is_rd_backpressure() {
            // back-pressure is still eanbled
            if buf.len() > half {
                return;
            }
            st.flags.unset_all_read_flags();
        } else {
            st.flags.unset_read_ready();
        }

        if st.flags.is_read_paused() {
            st.wake_read_task();
            st.flags.unset_read_paused();
        }
    }

    /// Make sure buffer has enough free space
    pub fn resize_read_buf(&self, buf: &mut BytesMut) {
        self.0.cfg.read_buf().resize(buf);
    }

    #[doc(hidden)]
    #[deprecated(since = "3.10.3", note = "Use .notify_disapatcher()")]
    /// Wakeup dispatcher
    pub fn wake(&self) {
        self.notify_dispatcher();
    }

    /// Wakeup dispatcher
    pub fn notify_dispatcher(&self) {
        log::trace!("{}: Timer, notify dispatcher", self.tag());
        self.0.wake_dispatch_task();
    }

    /// Wakeup dispatcher and send keep-alive error
    pub fn notify_timeout(&self) {
        self.0.notify_timeout();
    }

    /// current timer handle
    pub fn timer_handle(&self) -> TimerHandle {
        self.0.timeout.get()
    }

    /// Start timer
    pub fn start_timer(&self, timeout: Seconds) -> TimerHandle {
        let cur_hnd = self.0.timeout.get();

        if timeout.is_zero() {
            if cur_hnd.is_set() {
                self.0.timeout.set(TimerHandle::ZERO);
                cur_hnd.unregister(self);
            }
            TimerHandle::ZERO
        } else if cur_hnd.is_set() {
            let hnd = cur_hnd.update(timeout, self);
            if hnd != cur_hnd {
                log::trace!("{}: Update timer {:?}", self.tag(), timeout);
                self.0.timeout.set(hnd);
            }
            hnd
        } else {
            log::trace!("{}: Start timer {:?}", self.tag(), timeout);
            let hnd = TimerHandle::register(timeout, self);
            self.0.timeout.set(hnd);
            hnd
        }
    }

    /// Stop timer
    pub fn stop_timer(&self) {
        let hnd = self.0.timeout.get();
        if hnd.is_set() {
            log::trace!("{}: Stop timer", self.tag());
            self.0.timeout.set(TimerHandle::ZERO);
            hnd.unregister(self);
        }
    }

    /// Notify when io stream get disconnected
    pub fn on_disconnect(&self) -> crate::OnDisconnect {
        crate::OnDisconnect::new(self.0.clone())
    }

    /// Call handle write method, returns true if
    /// `write-paused` is still set
    fn call_write(&self) -> WakeWriteTask {
        if let Some(hnd) = self.0.handle.take() {
            let ctx = unsafe { &*(ptr::from_ref(self).cast::<IoContext>()) };
            hnd.write(ctx);
            self.0.handle.set(Some(hnd));
        }
        if !self.0.flags.is_write_paused() || self.0.flags.is_wr_send_scheduled() {
            WakeWriteTask::No
        } else {
            WakeWriteTask::Yes
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum WakeWriteTask {
    Yes,
    No,
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
        assert!(state.flags().is_terminated());

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::from(server);

        client.read_error(io::Error::other("err"));
        let res = poll_fn(|cx| Poll::Ready(state.poll_recv(&BytesCodec, cx))).await;
        if let Poll::Ready(msg) = res {
            assert!(msg.is_err());
            assert!(state.flags().is_terminated());
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
        assert!(state.flags().is_terminated());

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::from(server);
        state.terminate();
        assert!(state.flags().is_stopping());
        assert!(state.flags().is_terminated());
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
        assert!(state.encode_bytes(Bytes::from_static(BIN)).is_err());
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
            let nbytes = ctx.read_dst_size();
            let result = self.layer.process_read_buf(ctx);
            let nbytes2 = ctx.read_dst_size();
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
        let io = Io::from(server)
            .map_filter(|layer| Counter {
                layer,
                idx: 1,
                in_bytes: in_bytes.clone(),
                out_bytes: out_bytes.clone(),
                read_order: read_order.clone(),
                write_order: write_order.clone(),
            })
            .seal();

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
        assert_eq!(state.0.buffer.with_write_dst(|b| b.len()), 0);

        // refs
        assert_eq!(Rc::strong_count(&in_bytes), 3);
        drop(state);
        assert_eq!(Rc::strong_count(&in_bytes), 1);
        assert_eq!(*read_order.borrow(), &[1, 2][..]);
        assert_eq!(*write_order.borrow(), &[1, 2, 1, 2, 1, 2][..]);
    }
}
