use std::task::{Context, Poll};
use std::{fmt, future::poll_fn, hash, io, ptr};

#[cfg(feature = "tracker")]
use ntex_bytes::BytesTracker;
use ntex_bytes::{BytePageSize, BytesMut};

#[cfg(feature = "tracker")]
use crate::cfg::{BufTracker, IoConfig};
use crate::filter::Filter;
use crate::metrics::IoMetrics;
use crate::ops::Id;
use crate::utils::{WriteDeadline, write_timed_out};
use crate::waiters::{TAG_WRITE, WriteGuard};

use super::{Close, IoState, RD_SHRINK_BATCHES};

impl IoState {
    pub(crate) fn id(&self) -> Id {
        self.id.get()
    }

    pub(crate) fn tag(&self) -> &'static str {
        self.cfg.tag()
    }

    /// Checks whether the `Io` that owned the filter chain has been dropped.
    pub(crate) fn is_io_dropped(&self) -> bool {
        !self.filter.is_set()
    }

    pub(crate) fn filter(&self) -> &dyn Filter {
        self.filter.get()
    }

    pub(crate) fn notify_timeout(&self) {
        if self.flags.check_dispatcher_timeout_unset() {
            self.wake_dispatch_task();
            log::trace!("{}: Timer, notify dispatcher", self.cfg.tag());
        }
    }

    /// Wakes the disconnect waiters and all public waker slots.
    pub(crate) fn notify_disconnect(&self) {
        self.extensions.wake_all();
    }

    /// Get the current I/O error.
    pub(crate) fn error(&self) -> Option<io::Error> {
        if let Some(err) = self.error.take() {
            let cloned = if let Some(code) = err.raw_os_error() {
                io::Error::from_raw_os_error(code)
            } else {
                io::Error::new(err.kind(), format!("{err}"))
            };
            self.error.set(Some(cloned));
            Some(err)
        } else {
            None
        }
    }

    /// Returns the current I/O error, or creates a `NotConnected` error.
    pub(crate) fn error_or_disconnected(&self) -> io::Error {
        self.error()
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Disconnected"))
    }

    pub(crate) fn filters_stopped(&self) {
        // the shutdown deadline carries over into the transport shutdown
        // phase, so that a single `shutdown_timeout` bounds both phases
        self.flags.enter_transport_shutdown();
        self.wake_read_task();
        self.wake_write_task();
        self.wake_dispatch_task();
        self.wake_write_waiters();
    }

    fn set_error(&self, err: Option<io::Error>) {
        if let Some(err) = err {
            if let Some(current) = self.error.take() {
                self.error.set(Some(current));
            } else {
                self.error.set(Some(err));
            }
        }
    }

    pub(crate) fn set_shutdown_error(&self, err: io::Error) {
        self.set_error(Some(err));
    }

    /// Force-closes the connection, aborting it instead of closing gracefully.
    ///
    /// This is the only path that makes the transport report
    /// [`Readiness::Terminate`](crate::Readiness::Terminate). Terminations that
    /// come from a failure rather than from an explicit request go through
    /// [`terminate_connection`](Self::terminate_connection) and let the
    /// transport close the connection gracefully.
    pub(crate) fn force_close_connection(&self) {
        self.begin_terminate(None, true);
    }

    pub(crate) fn terminate_connection(&self, err: Option<io::Error>) {
        self.begin_terminate(err, false);
    }

    /// Moves the connection onto the termination path and runs the teardown
    /// work, which only the first call to get here performs.
    fn begin_terminate(&self, err: Option<io::Error>, force: bool) {
        self.set_error(err);
        if self.flags.begin_terminate(force) {
            log::trace!("{}: Terminate io", self.cfg.tag());
            // buffers held by the transport are gone with it
            self.wr_inflight.set(0);
            self.wake_read_task();
            self.wake_write_task();
            self.wake_dispatch_task();
            self.wake_write_waiters();
            self.handle.take();
        }
        if force {
            // an aborted transport does not write buffered output
            self.discard_output();
        }
    }

    pub(crate) fn stop_connection(&self, err: Option<io::Error>) {
        if !self.flags.is_closed() {
            log::trace!("{}: Stop io with error {:?}", self.cfg.tag(), err);
            self.set_error(err);
            self.flags.set_stopped();
            // nothing can deliver buffered output anymore, buffers held by
            // the transport are gone with it
            self.discard_output();
            self.wake_read_task();
            self.wake_write_task();
            self.wake_dispatch_task();
            self.wake_write_waiters();
            self.notify_disconnect();
            self.handle.take();
        }
    }

    /// Frees output that can not be delivered anymore, see
    /// [`is_output_discarded`](Self::is_output_discarded).
    fn discard_output(&self) {
        self.wr_inflight.set(0);
        // with a buffer in use, its user updates the buffered size afterwards
        if self.buffer.discard_write() {
            self.sync_write_buffered();
        }
    }

    /// Whether the transport is gone or aborts the connection, so that output
    /// can not be delivered anymore.
    pub(crate) fn is_output_discarded(&self) -> bool {
        self.flags.is_closed() || self.flags.is_force_closing()
    }

    /// Gracefully shuts down the read and write I/O tasks.
    pub(crate) fn start_shutdown(&self) {
        if self.flags.is_active() {
            log::trace!("{}: Initiate io shutdown {:?}", self.cfg.tag(), self.flags);
            self.flags.enter_filters_stopping();
            self.wake_read_task();
            self.wake_write_task();
        }
    }

    /// Acquires an empty read buffer of the connection's read page size from
    /// the thread-local page cache.
    pub(crate) fn get_read_buf(&self) -> BytesMut {
        #[cfg(feature = "tracker")]
        if let Some(t) = self.buf_tracker() {
            return BytesMut::with_page_size_in(self.rd_size.get(), t);
        }
        BytesMut::with_page_size(self.rd_size.get())
    }

    /// Returns the tracker charged with new read buffers.
    ///
    /// A shared tracker is borrowed from the config, a per-connection
    /// tracker is created on first use. The reference must not be used after
    /// [`Io::set_config`](crate::Io::set_config).
    #[cfg(feature = "tracker")]
    pub(crate) fn buf_tracker(&self) -> Option<&BytesTracker> {
        match self.cfg.buf_tracker() {
            BufTracker::Disabled => None,
            BufTracker::Shared(t) => Some(t),
            BufTracker::PerConnection => {
                let slot = self.rd_tracker.get();
                // SAFETY: the slot is written only while it is empty or by
                // `set_config`, so no reference to its value is live
                unsafe {
                    if (*slot).is_none() {
                        *slot = Some(BytesTracker::new());
                    }
                    (*slot).as_ref()
                }
            }
        }
    }

    /// Switches the read buffer tracker to the mode of `cfg`, before it
    /// replaces the current config.
    ///
    /// The read buffers held by the stack are charged to the new tracker.
    #[cfg(feature = "tracker")]
    pub(super) fn update_buf_tracker(&self, cfg: &IoConfig) {
        // SAFETY: `set_config` requires that no reference obtained from
        // `buf_tracker` is live
        let slot = unsafe { &mut *self.rd_tracker.get() };
        match cfg.buf_tracker() {
            BufTracker::Disabled => *slot = None,
            BufTracker::Shared(t) => {
                *slot = None;
                self.buffer.set_buf_tracker(t);
            }
            BufTracker::PerConnection => {
                if !matches!(self.cfg.buf_tracker(), BufTracker::PerConnection) {
                    let t = slot.insert(BytesTracker::new());
                    self.buffer.set_buf_tracker(t);
                }
            }
        }
    }

    /// Returns the page size of new read buffers.
    #[cfg(test)]
    pub(crate) fn read_size(&self) -> BytePageSize {
        self.rd_size.get()
    }

    /// Resets the read page size to the configured min.
    pub(super) fn reset_read_size(&self) {
        self.rd_size.set(self.cfg.read_size_min());
        self.rd_batch.set(0);
        self.rd_small.set(0);
    }

    /// Adapts the read page size to a transport read of `nbytes`.
    ///
    /// Reads that fill their buffer most likely leave more input behind, so
    /// they and the read that follows them form one batch. A batch larger
    /// than the page capacity grows the page size to fit it, up to the
    /// configured max. After [`RD_SHRINK_BATCHES`] batches in a row that fit
    /// in half of the next smaller page, the page size shrinks by one step,
    /// down to the configured min.
    pub(crate) fn track_read(&self, nbytes: usize, full: bool) {
        let batch = self.rd_batch.get().saturating_add(as_u32(nbytes));
        let size = self.rd_size.get();
        if full {
            self.rd_batch.set(batch);
            if batch as usize > size.capacity() {
                self.grow_read_size(batch as usize);
            }
            return;
        }

        self.rd_batch.set(0);
        let batch = batch as usize;
        if batch == 0 {
            // nothing was read since the previous batch ended
        } else if batch > size.capacity() {
            self.grow_read_size(batch);
        } else if size != self.cfg.read_size_min() && batch < size.prev().capacity() / 2 {
            let small = self.rd_small.get() + 1;
            if small < RD_SHRINK_BATCHES {
                self.rd_small.set(small);
            } else {
                let min = self.cfg.read_size_min();
                let prev = size.prev();
                self.rd_size.set(if prev.capacity() < min.capacity() {
                    min
                } else {
                    prev
                });
                self.rd_small.set(0);
            }
        } else {
            self.rd_small.set(0);
        }
    }

    fn grow_read_size(&self, batch: usize) {
        let max = self.cfg.read_size_max();
        let size = BytePageSize::for_capacity(batch);
        let size = if size == BytePageSize::Unset || size.capacity() > max.capacity() {
            max
        } else {
            size
        };
        if size.capacity() > self.rd_size.get().capacity() {
            self.rd_size.set(size);
        }
        self.rd_small.set(0);
    }

    pub(crate) fn is_rd_backpressure_needed(&self, size: usize) -> bool {
        size >= self.cfg.read_backpressure()
    }

    pub(crate) fn is_wr_backpressure_needed(&self, size: usize) -> bool {
        size >= self.cfg.write_backpressure()
    }

    pub(crate) fn should_disable_rd_backpressure(&self, size: usize) -> bool {
        size <= self.cfg.read_half()
    }

    pub(crate) fn should_disable_wr_backpressure(&self, size: usize) -> bool {
        size <= self.cfg.write_half()
    }

    /// Total output that has not reached the peer yet.
    ///
    /// This is the buffered output plus whatever the transport has taken
    /// ownership of but not written out. Flush completion, write
    /// back-pressure and the shutdown drain are all decided on this value,
    /// because until it reaches zero the peer has not seen everything.
    pub(crate) fn write_outstanding(&self) -> usize {
        self.buffer.write_buf_size() + self.wr_inflight.get() as usize
    }

    /// Output that has reached the transport-facing buffer but not the peer.
    ///
    /// Output a filter holds back, for example application data during a TLS
    /// renegotiation, is excluded. It can not drain until more input is read,
    /// so it must not keep reads paused.
    pub(crate) fn transport_outstanding(&self) -> usize {
        self.buffer.write_dst_size() + self.wr_inflight.get() as usize
    }

    /// Records bytes taken by, or returned from, the transport.
    #[inline]
    pub(crate) fn metrics(&self) -> &IoMetrics {
        // SAFETY: replaced only by `Io::set_config()`, which requires that no
        // reference into the connection state is live
        unsafe { &*self.metrics.get() }
    }

    pub(crate) fn track_wr_inflight(&self, before: usize, after: usize) {
        let inflight = self.wr_inflight.get();
        if after < before {
            self.wr_inflight
                .set(inflight.saturating_add(as_u32(before - after)));
        } else {
            // the transport returned unwritten output to the buffer
            self.wr_inflight
                .set(inflight.saturating_sub(as_u32(after - before)));
        }
    }

    /// Records output that reached the peer.
    pub(crate) fn wr_inflight_written(&self, written: usize) {
        self.wr_inflight
            .set(self.wr_inflight.get().saturating_sub(as_u32(written)));
    }

    pub(crate) fn set_wr_backpressure(&self) {
        if self.flags.set_wr_backpressure() {
            self.metrics().wr_backpressure_on();
        }
    }

    /// Enables write back-pressure once outstanding output reaches the high
    /// watermark.
    ///
    /// Besides application writes, this is checked after deferred filter
    /// processing, which can grow the output, for example a filter that
    /// expands its input. Only the flag is set, no write is started.
    pub(crate) fn enable_wr_backpressure_if_needed(&self) {
        if !self.flags.is_wr_backpressure()
            && self.is_wr_backpressure_needed(self.write_outstanding())
        {
            self.set_wr_backpressure();
            self.wake_dispatch_task();
        }
    }

    pub(crate) fn unset_wr_backpressure(&self) {
        if self.flags.unset_wr_backpressure() {
            self.metrics().wr_backpressure_off();
        }
    }

    pub(crate) fn unset_wr_backpressure_and_flush(&self) {
        if self.flags.unset_wr_backpressure_and_flush() {
            self.metrics().wr_backpressure_off();
        }
    }

    pub(crate) fn set_read_ready_and_backpressure(&self) {
        if self.flags.set_read_ready_and_backpressure() {
            self.metrics().rd_backpressure_on();
        }
    }

    pub(crate) fn unset_read_ready_and_backpressure(&self) {
        if self.flags.unset_read_ready_and_backpressure() {
            self.metrics().rd_backpressure_off();
        }
    }

    /// Reports the current read buffer size and capacity to the metrics.
    ///
    /// Must not be called while a read buffer is borrowed, its size would be
    /// reported as zero.
    pub(crate) fn sync_read_buffered(&self) {
        let (size, cap) = self.buffer.read_buf_usage();
        let size = u32::try_from(size).unwrap_or(u32::MAX);
        let cap = u32::try_from(cap).unwrap_or(u32::MAX);
        let prev = self.rd_buffered.replace(size);
        let prev_cap = self.rd_capacity.replace(cap);
        self.metrics()
            .update_read_buffered(prev, size, prev_cap, cap);
    }

    /// Reports the current write buffer size to the metrics.
    pub(crate) fn sync_write_buffered(&self) {
        let size = u32::try_from(self.write_outstanding()).unwrap_or(u32::MAX);
        let prev = self.wr_buffered.replace(size);
        self.metrics().update_write_buffered(prev, size);
    }

    pub(crate) fn wake_read_task(&self) {
        self.read_task.wake();
    }

    /// Clears the read pause and wakes the read task.
    ///
    /// The flag is cleared first: a waker may poll the read task inline, and
    /// would otherwise see the stale pause and park without another wakeup.
    pub(crate) fn resume_read_task(&self) {
        self.flags.unset_read_paused();
        self.wake_read_task();
    }

    pub(crate) fn wake_write_task(&self) {
        #[cfg(feature = "trace")]
        log::trace!("{}: Wake write task, flags:{:?}", self.tag(), self.flags);
        self.write_task.wake();
    }

    pub(crate) fn wake_dispatch_task(&self) {
        self.dispatch_task.wake();
    }

    pub(crate) fn wake_write_waiters(&self) {
        self.extensions.wake(TAG_WRITE);
    }

    /// Returns `Some` once more output can be written or the connection is gone.
    pub(crate) fn check_write_ready(&self) -> Option<io::Result<()>> {
        if self.flags.is_peer_gone() {
            Some(Err(self.error_or_disconnected()))
        } else if !self.flags.is_wr_backpressure()
            || self.should_disable_wr_backpressure(self.write_outstanding())
        {
            Some(Ok(()))
        } else {
            None
        }
    }

    pub(crate) async fn write_ready(&self) -> io::Result<()> {
        if let Some(res) = self.check_write_ready() {
            return res;
        }

        let waiter = WriteGuard::new(&self.extensions);
        let mut deadline = WriteDeadline::new(self.cfg.write_timeout());
        poll_fn(|cx| {
            if let Some(res) = self.check_write_ready() {
                Poll::Ready(res)
            } else if deadline.poll_expired(cx) {
                Poll::Ready(Err(write_timed_out()))
            } else {
                // a woken waiter registers the waker again
                waiter.register(cx);
                Poll::Pending
            }
        })
        .await
    }

    /// Waits for `f`, bounded by the configured write timeout.
    pub(crate) async fn with_write_timeout<T, F>(&self, mut f: F) -> io::Result<T>
    where
        F: FnMut(&mut Context<'_>) -> Poll<io::Result<T>>,
    {
        let mut deadline = WriteDeadline::new(self.cfg.write_timeout());
        poll_fn(|cx| match f(cx) {
            Poll::Ready(res) => Poll::Ready(res),
            Poll::Pending if deadline.poll_expired(cx) => Poll::Ready(Err(write_timed_out())),
            Poll::Pending => Poll::Pending,
        })
        .await
    }
}

impl Drop for IoState {
    fn drop(&mut self) {
        // a connection can go away under backpressure
        if self.flags.is_rd_backpressure() {
            self.metrics().rd_backpressure_off();
        }
        if self.flags.is_wr_backpressure() {
            self.metrics().wr_backpressure_off();
        }
        self.metrics()
            .update_read_buffered(self.rd_buffered.get(), 0, self.rd_capacity.get(), 0);
        self.metrics()
            .update_write_buffered(self.wr_buffered.get(), 0);
        match self.close.get() {
            Close::Unset => {
                let io_error = self.error.get_mut().is_some();
                self.metrics().closed(None, io_error);
            }
            Close::Reason(reason) => self.metrics().closed(Some(reason), false),
            Close::Uncounted => (),
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
        (ptr::from_ref(self) as usize).hash(state);
    }
}

impl fmt::Debug for IoState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let err = self.error.take();
        let res = f
            .debug_struct("IoState")
            .field("id", &self.id)
            .field("flags", &self.flags)
            .field("filter", &self.filter.is_set())
            .field("timeout", &self.timeout)
            .field("error", &err)
            .field("buffer", &self.buffer)
            .field("cfg", &self.cfg)
            .finish();
        self.error.set(err);
        res
    }
}

/// Saturating conversion used for the in-flight write counter.
fn as_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}
