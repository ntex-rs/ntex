use std::{any, fmt, hash, io, ptr};

use ntex_bytes::{BytePage, BytePages, BytesMut};
use ntex_codec::{Decoder, Encoder};
use ntex_service::cfg::SharedCfg;
use ntex_util::time::Seconds;

use crate::ops::{Id, Iops, TimerHandle};
use crate::{Decoded, Filter, FilterBuf, Flags, Handle, IoConfig, IoContext, IoRef, types};

impl IoRef {
    #[inline]
    /// Gets the ID.
    pub fn id(&self) -> Id {
        self.0.id()
    }

    #[inline]
    /// Gets the I/O tag.
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[doc(hidden)]
    /// Gets the state flags. (for debug purpose only)
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
    /// Checks whether the I/O stream is closed or closing.
    ///
    /// This becomes `true` as soon as graceful shutdown or force termination
    /// starts, so buffered output may still be flushing. Use
    /// [`is_stopping`](Self::is_stopping) and
    /// [`is_terminating`](Self::is_terminating) to tell the two paths apart.
    pub fn is_closed(&self) -> bool {
        self.0.flags.is_closed()
    }

    #[inline]
    /// Checks whether the transport read half reached clean EOF.
    ///
    /// Buffered input remains available and the write half may still be used.
    pub fn is_read_eof(&self) -> bool {
        self.0.flags.is_read_eof()
    }

    #[inline]
    /// Checks whether the transport entered graceful shutdown.
    ///
    /// This state remains set after backend teardown completes.
    pub fn is_stopping(&self) -> bool {
        self.0.flags.is_stopping()
    }

    #[inline]
    /// Checks whether the stream entered the force-termination path.
    ///
    /// This becomes `true` after [`terminate`](Self::terminate) is called or
    /// an I/O or filter error requests immediate termination. Unlike graceful
    /// shutdown, pending application work is not drained. The value remains
    /// `true` after backend teardown completes so callers can distinguish a
    /// terminated stream from one that closed gracefully.
    pub fn is_terminating(&self) -> bool {
        self.0.flags.is_terminating()
    }

    #[inline]
    /// Checks whether read back-pressure is enabled.
    ///
    /// This becomes `true` once unread data in the application-facing read
    /// buffer reaches the configured high watermark, which parks the transport
    /// read task.
    ///
    /// Two different paths release it. Consuming through
    /// [`decode`](Self::decode), [`with_buf`](Self::with_buf),
    /// [`with_read_src`](Self::with_read_src) or
    /// [`with_read_dst`](Self::with_read_dst) releases it once the buffer has
    /// fallen to at most half the high watermark. Asking for more input through
    /// [`Io::poll_read_more`](crate::Io::poll_read_more), and the methods built
    /// on it, releases it immediately however much data is still buffered.
    pub fn is_rd_backpressure(&self) -> bool {
        self.0.flags.is_rd_backpressure()
    }

    #[inline]
    /// Checks whether write back-pressure is enabled.
    ///
    /// This becomes `true` once unwritten data in the transport-facing write
    /// buffer reaches the configured high watermark. Draining enough of the
    /// buffer releases it.
    ///
    /// Nothing enforces the signal: encoding continues to succeed while it is
    /// set. Producers that are not driven by a dispatcher should check this
    /// before encoding more, or the write buffer grows without bound. See
    /// [`encode`](Self::encode).
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
    /// terminated without any graceful period, and whatever is still buffered
    /// is discarded.
    ///
    /// The transport aborts the connection instead of closing it gracefully, so
    /// the peer most likely observes an `RST` rather than a clean end of
    /// stream, and output that has not been acknowledged yet is lost. That is
    /// what keeps a truncated response distinguishable from a complete one, but
    /// it also means this must not be used to end a connection normally. Use
    /// [`close`](Self::close) for that.
    pub fn terminate(&self) {
        log::trace!("{}: Terminate io stream object", self.tag());
        self.0.force_close_connection();
    }

    /// Queries filter-specific data.
    pub fn query<T: 'static>(&self) -> types::QueryItem<T> {
        types::QueryItem::new(self.filter().query(any::TypeId::of::<T>()))
    }

    #[inline]
    /// Encodes an item into the write buffer.
    ///
    /// This method reports codec errors only. Any `io::Error` produced while
    /// buffering is discarded: if the connection is already closing or closed
    /// the item is not encoded, and a transport or filter error raised by an
    /// eager backend write is dropped. Such errors remain observable later
    /// through [`crate::Io::poll_flush`] or [`crate::Io::poll_recv`]. Use
    /// [`encode_slice`](Self::encode_slice) or
    /// [`encode_bytes`](Self::encode_bytes) when they must be observed at the
    /// call site.
    ///
    /// # Back-pressure is advisory
    ///
    /// Encoding never blocks and never refuses. Once buffered output reaches
    /// the configured high watermark this arms write back-pressure and wakes
    /// the dispatch task, but the item is still buffered and `Ok` is still
    /// returned. A caller that keeps encoding without consulting
    /// [`is_wr_backpressure`](Self::is_wr_backpressure), or awaiting
    /// [`Io::poll_status_update`](crate::Io::poll_status_update) or
    /// [`Io::poll_flush`](crate::Io::poll_flush), will grow the write buffer
    /// without bound, because a slow peer cannot slow the producer down on its
    /// own. Honouring the signal is the caller's responsibility.
    pub fn encode<U>(&self, item: U::Item, codec: &U) -> Result<(), <U as Encoder>::Error>
    where
        U: Encoder,
    {
        self.with_write_src(|buf| codec.encodev(item, buf))
            .unwrap_or_else(|_| Ok(()))
    }

    #[inline]
    /// Encodes the slice into the write buffer.
    ///
    /// If this triggers an eager backend write, any transport or filter error
    /// from that write is returned immediately.
    ///
    /// Write back-pressure is advisory here too; see [`encode`](Self::encode).
    pub fn encode_slice(&self, src: &[u8]) -> io::Result<()> {
        self.with_write_src(|buf| buf.extend_from_slice(src))
    }

    #[inline]
    /// Writes bytes to the write buffer.
    ///
    /// If this triggers an eager backend write, any transport or filter error
    /// from that write is returned immediately.
    ///
    /// Write back-pressure is advisory here too; see [`encode`](Self::encode).
    pub fn encode_bytes<B>(&self, src: B) -> io::Result<()>
    where
        BytePage: From<B>,
    {
        self.with_write_src(|buf| buf.append(src))
    }

    /// Attempts to decode a frame from the read buffer.
    ///
    /// This mutates the read state: it clears read readiness, and consuming
    /// enough bytes may release read backpressure. It also cancels a pause
    /// installed by [`Io::poll_read_pause`](crate::Io::poll_read_pause) and wakes the transport
    /// read task.
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
    ///
    /// `Decoded::consumed` reports the bytes taken by this attempt and
    /// `Decoded::remains` the bytes left in the application-facing read
    /// buffer.
    ///
    /// Like [`decode`](Self::decode), this mutates the read state: it clears
    /// read readiness, may release read backpressure, and cancels a pause
    /// installed by [`Io::poll_read_pause`](crate::Io::poll_read_pause).
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
        self.consolidate_write_state(true)
    }

    pub(crate) fn ops_send_buf(&self) {
        let st = &self.0;
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: ops-send == buf:{} flags:{:?}",
            st.tag(),
            st.buffer.write_buf_size(),
            st.flags
        );

        if st.flags.is_wr_send_scheduled() {
            st.flags.unset_wr_send_scheduled();

            if st.flags.is_write_paused() {
                // call `Handle::write()`.
                // if write task is not paused, io write is pending
                // need to wake write task for io completeion
                if self.call_write() == WakeWriteTask::Yes {
                    st.wake_write_task();
                    st.flags.unset_write_paused();
                }
            } else {
                st.wake_write_task();
            }
        }
    }

    /// Provides temporary access to the outermost filter buffers.
    ///
    /// Filter callbacks run before and after `f`, and any produced write data
    /// is scheduled for delivery after the closure returns. Errors from an
    /// eager backend write are returned to the caller.
    ///
    /// The destination exposed by
    /// [`FilterBuf::with_read_buffers`](crate::FilterBuf::with_read_buffers) is
    /// the application-facing read destination, so consuming enough of it
    /// releases read backpressure and cancels an installed read pause.
    pub fn with_buf<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut FilterBuf<'_>) -> R,
    {
        self.with_callbacks(|cb| cb.before_processing(self));
        let result = self.0.buffer.with_filter(self, |ctx| ctx.with_buffer(f));
        self.with_callbacks(|cb| cb.after_processing(self));
        self.release_read_destination();

        self.consolidate_write_state(false)?;
        Ok(result)
    }

    /// Provides mutable access to the application-facing read destination.
    ///
    /// This holds the decoded bytes the application consumes; see
    /// [`with_read_src`](Self::with_read_src) for the transport-facing source.
    ///
    /// This mutates the read state whether or not `f` consumes anything. While
    /// read back-pressure is active nothing is released until the buffer has
    /// fallen to at most half the high watermark, so until then read readiness
    /// and any installed read pause are left in place. Once it has, or when
    /// back-pressure was not active, read readiness is cleared and a pause
    /// installed by [`Io::poll_read_pause`](crate::Io::poll_read_pause) is
    /// cancelled, waking the transport read task.
    ///
    /// Use [`crate::Io::poll_read_more`] rather than this method to check
    /// whether data is available.
    pub fn with_read_dst<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.0.buffer.with_read_dst(self, |buf| {
            let res = f(buf);
            self.update_read_destination(buf);
            res
        })
    }

    /// Provides mutable access to the application-facing write source.
    ///
    /// This holds the bytes the application produces; see
    /// [`with_write_dst`](Self::with_write_dst) for the transport-facing
    /// destination.
    ///
    /// Returns an error without invoking `f` if the connection is closing or
    /// closed. Data appended by `f` is scheduled for delivery. If that starts
    /// an eager backend write, its transport or filter error is returned.
    pub fn with_write_src<F, R>(&self, f: F) -> io::Result<R>
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        let st = &self.0;

        if st.flags.is_stopping_any() {
            if st.flags.is_closed() {
                Err(st.error_or_disconnected())
            } else {
                Err(io::Error::other("I/O stream is closing"))
            }
        } else {
            let result = st.buffer.with_write_src(f);
            self.consolidate_write_state(false)?;
            Ok(result)
        }
    }

    #[inline]
    /// Provides mutable access to the transport-facing read source.
    ///
    /// This is the buffer the transport fills; it is the counterpart of the
    /// application-facing destination exposed by
    /// [`with_read_dst`](Self::with_read_dst). Primarily intended for transport
    /// and filter implementations.
    ///
    /// Without a filter installed this is the same buffer as the
    /// application-facing destination, so consuming enough of it releases read
    /// backpressure and cancels an installed read pause. Unlike
    /// [`with_read_dst`](Self::with_read_dst) it never clears read readiness.
    pub fn with_read_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let result = self.0.buffer.with_read_src(self, f);
        self.release_read_destination();
        result
    }

    #[inline]
    /// Provides mutable access to the transport-facing write destination.
    ///
    /// This is the buffer the transport drains; it is the counterpart of the
    /// application-facing source exposed by
    /// [`with_write_src`](Self::with_write_src). Primarily intended for
    /// transport and filter implementations.
    pub fn with_write_dst<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.0.buffer.with_write_dst(f)
    }

    /// Schedules buffered output for delivery and updates write state.
    ///
    /// When output is buffered and the write task is paused, this either
    /// performs an eager in-place write through the transport handle or
    /// schedules a write operation. An eager write requires direct writes to be
    /// enabled and, unless `force` is set, at least
    /// [`IoConfig::write_buf_threshold`](crate::IoConfig::write_buf_threshold)
    /// bytes to be buffered; `force` makes any non-empty buffer eligible. A
    /// write operation is scheduled when an eager write leaves data behind, or
    /// when no eager write was attempted.
    ///
    /// Returns the connection error once the connection is stopping or
    /// terminating with an error set. This is how a failed eager write or
    /// filter reaches the caller, and it also prevents further eager writes on
    /// a connection that is already gone.
    ///
    /// Finally enables write back-pressure and wakes the dispatcher if buffered
    /// output has reached the configured high watermark. The size is re-read
    /// first because an eager write may have drained it.
    pub(crate) fn consolidate_write_state(&self, force: bool) -> io::Result<()> {
        let st = &self.0;

        // wake write task if needsed
        let size = st.buffer.write_buf_size();

        #[cfg(feature = "trace")]
        log::trace!("{}: write-upd == buf:{size} flags:{:?}", st.tag(), st.flags);

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
            if st.flags.is_direct_wr_enabled() && (force || size >= st.cfg.write_buf_threshold()) {
                // Send data in-place
                if self.call_write() == WakeWriteTask::Yes {
                    #[cfg(feature = "trace")]
                    log::trace!(
                        "{}: write-upd == schedule(more):{} flags:{:?}",
                        st.tag(),
                        st.buffer.write_buf_size(),
                        st.flags
                    );
                    if !st.flags.is_wr_send_scheduled() {
                        // More data needs to be sent
                        st.flags.set_wr_send_scheduled();
                        Iops::schedule_write(st.id());
                    }
                } else {
                    st.flags.unset_wr_send_scheduled();
                }
            } else if !st.flags.is_wr_send_scheduled() {
                #[cfg(feature = "trace")]
                log::trace!("{}: write-upd == schedule(too small)", st.tag());
                st.flags.set_wr_send_scheduled();
                Iops::schedule_write(st.id());
            }
        }

        if st.flags.is_stopping_any()
            && let Some(err) = st.error()
        {
            return Err(err);
        }

        // A direct write may have changed the amount of buffered data.
        // In-flight output counts too: it has not reached the peer yet.
        let size = st.write_outstanding();

        // Enable backpressure
        if !st.flags.is_wr_backpressure() && st.is_wr_backpressure_needed(size) {
            st.flags.set_wr_backpressure();
            st.wake_dispatch_task();
        }
        Ok(())
    }

    /// Updates read state after the application-facing destination was accessed.
    ///
    /// While read back-pressure is active nothing is released until `buf` has
    /// fallen to at most half the high watermark. Until then read readiness and
    /// any installed read pause are deliberately left in place, keeping the
    /// transport read task parked. Once it has, read readiness and
    /// back-pressure are cleared together; without back-pressure only read
    /// readiness is cleared.
    ///
    /// Whenever something is released, a pause installed by
    /// [`Io::poll_read_pause`](crate::Io::poll_read_pause) is cancelled and the
    /// transport read task is woken.
    ///
    /// See [`release_read_destination`](Self::release_read_destination) for the
    /// variant used when the caller may have drained a different buffer of the
    /// filter chain.
    fn update_read_destination(&self, buf: &mut BytesMut) {
        let st = &self.0;

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: read-upd == buf:{} flags:{:?}",
            st.tag(),
            buf.len(),
            st.flags
        );

        if st.flags.is_rd_backpressure() {
            // Keep reads paused until enough buffered data has been consumed.
            if !st.should_disable_rd_backpressure(buf.len()) {
                return;
            }
            st.flags.unset_read_ready_and_backpressure();
        } else {
            st.flags.unset_read_ready();
        }

        if st.flags.is_read_paused() {
            st.wake_read_task();
            st.flags.unset_read_paused();
        }
    }

    /// Releases read backpressure and any installed read pause.
    ///
    /// Used by the accessors that can drain the application-facing read
    /// destination without going through
    /// [`with_read_dst`](Self::with_read_dst). Unlike
    /// `update_read_destination()` this never clears read readiness, because
    /// the caller may have touched a different buffer of the chain. It only
    /// removes a stale pause, so it can never suppress a wakeup.
    fn release_read_destination(&self) {
        let st = &self.0;

        if st.flags.is_rd_backpressure() {
            if !st.should_disable_rd_backpressure(st.buffer.read_dst_size()) {
                return;
            }
            st.flags.unset_read_ready_and_backpressure();
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

    /// Wakeup dispatcher
    pub fn notify_dispatcher(&self) {
        log::trace!("{}: Timer, notify dispatcher", self.tag());
        self.0.wake_dispatch_task();
    }

    /// Wakeup dispatcher and send keep-alive error
    pub fn notify_timeout(&self) {
        self.0.notify_timeout();
    }

    /// Returns the currently registered dispatcher timer handle.
    ///
    /// [`TimerHandle::ZERO`] is returned when no timer is registered.
    pub fn timer_handle(&self) -> TimerHandle {
        self.0.timeout.get()
    }

    /// Starts or updates the dispatcher timer.
    ///
    /// The timer uses second-granularity deadlines. When it expires,
    /// [`poll_status_update`](crate::Io::poll_status_update) reports
    /// [`IoStatusUpdate::KeepAlive`](crate::IoStatusUpdate::KeepAlive).
    ///
    /// A zero timeout cancels the current timer but does not consume a timeout
    /// notification that has already been delivered. Use
    /// [`stop_timer`](Self::stop_timer) when leaving a protocol phase to also
    /// clear such a notification.
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

    /// Stops the timer and clears any pending timeout notification.
    pub fn stop_timer(&self) {
        self.0.flags.check_dispatcher_timeout();

        let hnd = self.0.timeout.get();
        if hnd.is_set() {
            log::trace!("{}: Stop timer", self.tag());
            self.0.timeout.set(TimerHandle::ZERO);
            hnd.unregister(self);
        }
    }

    /// Returns a future that resolves when the complete I/O stream disconnects.
    ///
    /// A clean peer read EOF does not resolve this future because the write
    /// half remains usable. It resolves once the transport backend reports
    /// that teardown has finished, which happens after local shutdown or
    /// force termination. [`terminate`](Self::terminate) requests that
    /// teardown but does not itself resolve the future.
    pub fn on_disconnect(&self) -> crate::OnDisconnect {
        crate::OnDisconnect::new(self.0.clone())
    }

    #[doc(hidden)]
    /// Register filter callbacks
    pub fn register_filter_callbacks<F: crate::IoCallbacks + 'static>(&self, f: F) {
        self.0.extensions.register_filter_callbacks(f);
    }

    /// Call handle write method, returns true if
    /// `write-paused` is still set
    fn call_write(&self) -> WakeWriteTask {
        if let Some(hnd) = self.0.handle.take() {
            self.0.flags.unset_write_paused();
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: call-write ({}), flags:{:?}",
                self.tag(),
                self.0.buffer.write_buf_size(),
                self.0.flags
            );
            let ctx = unsafe { &*(ptr::from_ref(self).cast::<IoContext>()) };
            hnd.write(ctx);
            self.restore_handle(hnd);
        }
        if self.0.flags.is_write_paused() {
            WakeWriteTask::No
        } else {
            WakeWriteTask::Yes
        }
    }

    /// Reinstalls the transport handle after a reentrant transport callback.
    ///
    /// The handle is taken for the duration of the call so that a nested
    /// `call_write()` cannot reenter the transport. If the
    /// callback terminated the connection, `terminate_connection()` and
    /// `stop_connection()` found the slot empty and could not release the
    /// transport, so the handle is dropped here instead of being reinstalled.
    /// A graceful shutdown keeps it: the write task still needs the transport
    /// to shut it down.
    fn restore_handle(&self, hnd: Box<dyn Handle>) {
        if self.0.flags.is_terminating() || self.0.flags.is_terminated() {
            drop(hnd);
        } else {
            self.0.handle.set(Some(hnd));
        }
    }

    pub(crate) fn with_callbacks<F>(&self, f: F)
    where
        F: FnOnce(&dyn crate::IoCallbacks),
    {
        self.0.extensions.with_callbacks(f);
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
    use ntex_util::future::{Either, lazy};
    use ntex_util::time::{Millis, sleep, timeout};

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
        assert_eq!(0, state.with_write_dst(|b| b.len()));
        state.encode_slice(b"test").unwrap();
        assert_eq!(4, state.with_write_dst(|b| b.len()));
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write(b"test");
        state.read_more().await.unwrap();
        let buf = state.decode(&BytesCodec).unwrap().unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        client.write_error(io::Error::other("err"));
        let err = state
            .send(Bytes::from_static(b"test"), &BytesCodec)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            Either::Right(ref err)
                if err.kind() == io::ErrorKind::Other && err.to_string() == "err"
        ));
        assert!(state.flags().is_terminated());

        let res = state.send(Bytes::from_static(b"test"), &BytesCodec).await;
        assert!(res.is_err());

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::from(server);
        state.terminate();
        assert!(state.flags().is_terminating());
        assert!(!state.flags().is_stopping());
        assert!(!state.flags().is_terminated());
        state.shutdown().await.unwrap();
        assert!(state.flags().is_terminated());
    }

    #[ntex::test]
    async fn zero_byte_write_reports_write_zero() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let state = Io::from(server);

        client.write_zero();
        let err = state
            .send(Bytes::from_static(b"test"), &BytesCodec)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Either::Right(ref err) if err.kind() == io::ErrorKind::WriteZero
        ));
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
        assert!(state.is_read_eof());
        assert!(!state.is_closed());
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Pending
        );
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter2).poll(cx)).await,
            Poll::Pending
        );

        timeout(Millis(1000), state.shutdown())
            .await
            .expect("stream shutdown did not complete")
            .unwrap();
        timeout(Millis(1000), waiter)
            .await
            .expect("disconnect waiter was not notified");
        timeout(Millis(1000), waiter2)
            .await
            .expect("cloned disconnect waiter was not notified");

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

        let mut waiter = state.on_disconnect();
        assert_eq!(
            lazy(|cx| Pin::new(&mut waiter).poll(cx)).await,
            Poll::Ready(())
        );
    }

    #[ntex::test]
    async fn write_to_closed_io() {
        let (_client, server) = IoTest::create();
        let state = Io::from(server);
        state.terminate();

        assert!(state.is_closed());
        assert!(state.encode_slice(TEXT.as_bytes()).is_err());
        assert!(state.encode_bytes(Bytes::from_static(BIN)).is_err());
        assert!(
            state
                .with_write_src(|buf| buf.extend_from_slice(BIN))
                .is_err()
        );
    }

    #[derive(Debug)]
    struct Counter<F> {
        layer: F,
        idx: usize,
        out_bytes: Rc<Cell<usize>>,
        read_order: Rc<RefCell<Vec<usize>>>,
        write_order: Rc<RefCell<Vec<usize>>>,
    }

    impl<F: Filter> Filter for Counter<F> {
        fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
            self.read_order.borrow_mut().push(self.idx);
            self.layer.process_read_buf(ctx)
        }

        fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
            self.write_order.borrow_mut().push(self.idx);
            ctx.with_buffer(|buf| {
                buf.with_write_buffers(|src, _| {
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
        let out_bytes = Rc::new(Cell::new(0));
        let read_order = Rc::new(RefCell::new(Vec::new()));
        let write_order = Rc::new(RefCell::new(Vec::new()));

        let (client, server) = IoTest::create();
        let io = Io::from(server)
            .map_filter(|layer| Counter {
                layer,
                idx: 1,
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

        assert_eq!(out_bytes.get(), 8);
    }

    #[ntex::test]
    async fn boxed_filter() {
        let out_bytes = Rc::new(Cell::new(0));
        let read_order = Rc::new(RefCell::new(Vec::new()));
        let write_order = Rc::new(RefCell::new(Vec::new()));

        let (client, server) = IoTest::create();
        let state = Io::from(server)
            .map_filter(|layer| Counter {
                layer,
                idx: 2,
                out_bytes: out_bytes.clone(),
                read_order: read_order.clone(),
                write_order: write_order.clone(),
            })
            .map_filter(|layer| Counter {
                layer,
                idx: 1,
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

        assert_eq!(out_bytes.get(), 16);
        assert_eq!(state.0.buffer.with_write_dst(|b| b.len()), 0);

        // refs
        assert_eq!(Rc::strong_count(&out_bytes), 3);
        drop(state);
        assert_eq!(Rc::strong_count(&out_bytes), 1);
        assert_eq!(*read_order.borrow(), &[1, 2][..]);
        assert_eq!(*write_order.borrow(), &[1, 2, 1, 2, 1, 2][..]);
    }
}
