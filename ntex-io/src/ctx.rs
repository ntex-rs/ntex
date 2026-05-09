use std::{fmt, io, task::Context, task::Poll};

use ntex_bytes::{BytePages, BytesMut};
use ntex_util::time::sleep;

use crate::{Flags, Id, IoRef, IoTaskStatus, Readiness, io::IoState};

/// Context for io read task
pub struct IoContext(IoRef);

impl fmt::Debug for IoContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoContext").field("io", &self.0).finish()
    }
}

impl IoContext {
    pub(crate) fn new(io: IoRef) -> Self {
        Self(io)
    }

    fn st(&self) -> &IoState {
        &self.0.0
    }

    #[doc(hidden)]
    #[inline]
    pub fn id(&self) -> Id {
        self.0.id()
    }

    #[inline]
    /// Io tag
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[doc(hidden)]
    /// Io flags.
    pub fn flags(&self) -> Flags {
        self.0.flags()
    }

    #[inline]
    /// Check readiness for read operations.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.shutdown_filters(cx);
        self.0.filter().poll_read_ready(cx)
    }

    #[inline]
    /// Check readiness for write operations.
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.0.filter().poll_write_ready(cx)
    }

    /// Stop io stream.
    pub fn stop(&self, e: Option<io::Error>) {
        self.st().terminate_connection(e);
    }

    /// Check if io stream is stopped.
    pub fn is_stopped(&self) -> bool {
        self.st().flags.is_stopped()
    }

    /// Get read buffer.
    pub fn get_read_buf(&self) -> BytesMut {
        let st = self.st();

        if st.flags.is_read_ready() {
            // The dispatcher has not consumed the read buffer yet,
            // so we must not modify it.
            st.get_read_buf()
        } else if let Some(mut buf) = st.buffer.get_read_buf() {
            self.0.resize_read_buf(&mut buf);
            buf
        } else {
            st.get_read_buf()
        }
    }

    /// Resize read buffer.
    pub fn resize_read_buf(&self, buf: &mut BytesMut) {
        self.0.resize_read_buf(buf);
    }

    /// Updates the read status.
    ///
    /// Status `Ok(None)` indicates the connection was disconnected.
    pub fn update_read_status(&self, status: io::Result<Option<BytesMut>>) -> IoTaskStatus {
        let st = self.st();
        let result = status.map(|buf| buf.map(|buffer| {
            let orig_size = st.buffer.read_destination_size();

            st.buffer.process_read_buf(&self.0, buffer).and_then(|()| {
                let size = st.buffer.read_destination_size();

                // dest buffer has new data, wake up dispatcher
                if size > orig_size {
                    if st.enable_rd_backpressure(size) {
                        log::trace!(
                            "{}: Io read buffer is too large {size}, enable read back-pressure",
                            st.tag(),
                        );
                        st.flags.set_read_ready_and_backpressure();
                    } else {
                        st.flags.set_read_ready();
                    }
                    log::trace!("{}: New {size} bytes available, wakeup dispatcher", st.tag());
                    st.dispatch_task.wake();
                } else {
                    if st.flags.is_rd_backpressure() {
                        // The read task is paused due to read back-pressure,
                        // but there is no new data in the top-most read buffer.
                        // We therefore need to wake the read task so it can
                        // continue reading more data; otherwise, it could
                        // remain blocked indefinitely.
                        st.flags.unset_read_ready();
                    }
                    if st.flags.is_read_notify() {
                        // In the case of a "notify" flag, we must wake the
                        // dispatch task if any data was read from the source.
                        st.dispatch_task.wake();
                    }
                }

                // While reading, the filter chain may have written some data.
                // In that case, the filters need to process the write buffers
                // and potentially wake the write task.
                if st.flags.is_wants_write() {
                    st.flags.unset_wants_write();
                    st.buffer.process_write_buf_force(&self.0)
                } else {
                    Ok(())
                }
            })
        }));

        match result {
            Ok(Some(Ok(()))) => {
                if st.flags.is_read_paused_or_backpressure() {
                    IoTaskStatus::Pause
                } else {
                    IoTaskStatus::Io
                }
            }
            Ok(None) => {
                st.terminate_connection(None);
                IoTaskStatus::Stop
            }
            Err(err) | Ok(Some(Err(err))) => {
                st.terminate_connection(Some(err));
                IoTaskStatus::Stop
            }
        }
    }

    /// Get write buffer.
    pub fn with_write_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        // write buffer processing could be delayed,
        // need to call filter chain for processing
        if let Err(e) = self.st().buffer.process_write_buf(&self.0) {
            self.st().terminate_connection(Some(e));
        }

        // access to output write buffer
        self.st().buffer.with_write_destination(|buffer| f(buffer))
    }

    /// Update write status.
    ///
    /// `Ok(true)` indicates that one or more bytes were successfully written
    /// to the I/O stream.
    pub fn update_write_status(&self, status: io::Result<usize>) -> IoTaskStatus {
        let st = &self.st();

        match status {
            Ok(written) => {
                let len = st.buffer.write_buffer_size();

                // write backpressure is enabled and write buf smaller than half
                let can_disable_wr_backpressure =
                    st.flags.is_wr_backpressure() && st.disable_wr_backpressure(len);
                // write flush is enabled, and write buffer is fully written
                let can_disable_flush = st.flags.is_write_flush() && len == 0;

                if can_disable_wr_backpressure || can_disable_flush {
                    st.dispatch_task.wake();
                }

                // wake up both tasks
                if written > 0 && st.flags.is_write_notify() {
                    st.flags.unset_write_notify();
                    st.read_task.wake();
                    st.write_task.wake();
                }

                if self.is_stopped() {
                    IoTaskStatus::Stop
                } else if len == 0 {
                    // all data has been written
                    st.flags.set_write_paused();
                    IoTaskStatus::Pause
                } else {
                    IoTaskStatus::Io
                }
            }
            Err(e) => {
                st.terminate_connection(Some(e));
                IoTaskStatus::Stop
            }
        }
    }

    /// Wait when io get closed or preparing for close.
    pub fn shutdown(&self, flush: bool, cx: &mut Context<'_>) -> Poll<()> {
        let st = self.st();

        if flush && !st.flags.is_stopped() {
            if st.flags.is_write_paused() {
                return Poll::Ready(());
            }
            st.flags.set_write_notify();
            st.read_task.register(cx.waker());
            st.write_task.register(cx.waker());
            Poll::Pending
        } else if !st.flags.is_closed() {
            st.read_task.register(cx.waker());
            st.write_task.register(cx.waker());
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    fn shutdown_filters(&self, cx: &mut Context<'_>) {
        let st = &self.st();
        if !st.flags.is_shutting_down_filters() {
            return;
        }
        let io = &self.0;

        match st.buffer.process_shutdown(io) {
            Ok(Poll::Ready(())) => {
                st.filters_stopped();
            }
            Ok(Poll::Pending) => {
                // if buffer is not consumed it is unlikely
                // that filter will properly complete shutdown
                if st.flags.is_read_paused() || st.flags.is_read_ready_and_backpressure() {
                    st.filters_stopped();
                } else {
                    // filter shutdown timeout
                    let timeout = st
                        .shutdown_timeout
                        .take()
                        .unwrap_or_else(|| sleep(st.cfg.disconnect_timeout()));
                    if timeout.poll_elapsed(cx).is_ready() {
                        st.filters_stopped();
                    } else {
                        st.shutdown_timeout.set(Some(timeout));
                    }
                }
                if let Err(err) = st.buffer.process_write_buf(io) {
                    st.terminate_connection(Some(err));
                }
            }
            Err(err) => {
                st.terminate_connection(Some(err));
            }
        }
    }
}

impl Clone for IoContext {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Io, testing::IoTest};

    #[ntex::test]
    async fn ctx_basics() {
        let (_, server) = IoTest::create();

        let state = Io::from(server);
        let ctx = IoContext::new(state.get_ref());
        let _ = ctx.flags();
        assert!(ctx.id() != Id::default());
        assert!(format!("{ctx:?}").contains("IoContext"));
    }
}
