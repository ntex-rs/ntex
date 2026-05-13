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
    /// Gets the I/O tag.
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[doc(hidden)]
    /// Gets the flags.
    pub fn flags(&self) -> Flags {
        self.0.flags()
    }

    #[inline]
    /// Checks readiness for read operations.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.shutdown_filters(cx);
        self.0.filter().poll_read_ready(cx)
    }

    #[inline]
    /// Checks readiness for write operations.
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.0.filter().poll_write_ready(cx)
    }

    /// Stops the I/O stream.
    pub fn stop(&self, e: Option<io::Error>) {
        self.st().terminate_connection(e);
    }

    /// Checks if the I/O stream is stopped.
    pub fn is_stopped(&self) -> bool {
        self.st().flags.is_closed()
    }

    /// Gets the read buffer.
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

    /// Resizes the read buffer.
    pub fn resize_read_buf(&self, buf: &mut BytesMut) {
        self.0.resize_read_buf(buf);
    }

    /// Updates the read status.
    ///
    /// Returns `Ok(Some(buf))` containing the read buffer.
    /// `Ok(None)` indicates that the connection has been disconnected.
    pub fn update_read_status(
        &self,
        buf: BytesMut,
        status: io::Result<usize>,
    ) -> IoTaskStatus {
        let st = self.st();
        let buf_len = buf.len();

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: update-read-status == {status:?} buf:{buf_len:?} orig:{:?} flags:{:?}",
            st.tag(),
            st.buffer.read_dst_size(),
            st.flags
        );

        // release read buffer
        st.buffer.set_read_buf(buf, self.0.cfg());

        // process read buf
        let result = status.map(|nbytes| {
            let orig_size = buf_len.saturating_sub(nbytes);

            if nbytes == 0 {
                return Ok(());
            }
            st.buffer.process_read_buf(&self.0, nbytes).map(|()| {
                let size = st.buffer.read_dst_size();

                // dest buffer has new data, wake up dispatcher
                if size > orig_size {
                    if st.is_rd_backpressure_needed(size) {
                        log::trace!("{}: Io read buffer is too large {size}, enable read back-pressure", st.tag(),);
                        st.flags.set_read_ready_and_backpressure();
                    } else {
                        st.flags.set_read_ready();
                    }
                    #[cfg(feature = "trace")]
                    log::trace!("{}: New {size} bytes available (orig:{orig_size}), wakeup dispatcher", st.tag());
                    st.wake_dispatch_task();
                }

                if st.flags.is_read_notify() {
                    // In the case of a "notify" flag, we must wake the
                    // dispatch task if any data was read from the source.
                    st.wake_dispatch_task();
                    st.flags.set_read_notifed();
                }
            })
        });

        match result {
            Ok(Ok(())) => {
                if st.flags.is_closed() {
                    // st.terminate_connection(None);
                    IoTaskStatus::Stop
                } else if st.flags.is_read_paused_or_backpressure() {
                    IoTaskStatus::Pause
                } else {
                    IoTaskStatus::Io
                }
            }
            Err(err) | Ok(Err(err)) => {
                st.terminate_connection(Some(err));
                IoTaskStatus::Stop
            }
        }
    }

    /// Gets the write buffer.
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
        self.st().buffer.with_write_dst(|buffer| f(buffer))
    }

    /// Updates the write status.
    ///
    /// `Ok(true)` indicates that one or more bytes were successfully written
    /// to the I/O stream.
    pub fn update_write_status(&self, status: io::Result<bool>) -> IoTaskStatus {
        let st = &self.st();

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: update-write-status == {status:?} buf:{} flags:{:?}",
            st.tag(),
            st.buffer.write_buf_size(),
            st.flags
        );

        match status {
            Ok(written) => {
                let len = st.buffer.write_buf_size();
                // full flush mode is enabled
                if st.flags.is_write_flush() {
                    // the write buffer must be fully written
                    if len == 0 {
                        st.wake_dispatch_task();
                    }
                } else if st.flags.is_wr_backpressure()
                    && st.should_disable_wr_backpressure(len)
                {
                    // Write backpressure is active and the write buffer is below half capacity.
                    st.wake_dispatch_task();
                }

                // wake up both tasks
                if written && st.flags.is_write_notify() {
                    st.flags.unset_write_notify();
                    st.wake_read_task();
                    st.wake_write_task();
                }

                if st.flags.is_closed() {
                    IoTaskStatus::Stop
                } else if len == 0 {
                    // all data has been written
                    st.flags.set_write_paused();
                    if st.flags.is_stopping_filters() {
                        st.wake_read_task();
                    }
                    IoTaskStatus::Pause
                } else {
                    st.flags.unset_write_paused();
                    IoTaskStatus::Io
                }
            }
            Err(e) => {
                st.terminate_connection(Some(e));
                IoTaskStatus::Stop
            }
        }
    }

    /// Waits for the I/O stream to close or begin closing.
    pub fn shutdown(&self, flush: bool, cx: &mut Context<'_>) -> Poll<()> {
        let st = self.st();
        if flush && !st.flags.is_stopping() {
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

        let ready = match st.buffer.process_shutdown(&self.0) {
            Ok(Poll::Ready(())) => true,
            Ok(Poll::Pending) => false,
            Err(err) => {
                st.terminate_connection(Some(err));
                return;
            }
        };
        self.0.update_write_destination();

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: shutdown filters, done:{ready:?} buf-len:{:?}, flags:{:?}",
            st.tag(),
            st.buffer.write_buf_size(),
            st.flags,
        );

        // filters are shutdown and write task is paused
        if ready && st.flags.is_write_paused() && !st.flags.is_wr_send_scheduled() {
            st.filters_stopped();
        } else if st.flags.is_read_paused() || st.flags.is_read_ready_and_backpressure() {
            // if buffer is not consumed it is unlikely
            // that filter will properly complete shutdown
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
