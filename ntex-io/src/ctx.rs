use std::{fmt, io, task::Context, task::Poll};

use ntex_bytes::{BytePages, BytesMut};
use ntex_util::time::sleep;

use crate::{Flags, Id, IoRef, IoTaskStatus, Readiness, io::IoState};

/// Connection context shared with transport read and write tasks.
///
/// Transport implementations obtain buffers from this context, perform
/// nonblocking I/O, and return completion through
/// [`update_read_status`](Self::update_read_status) and
/// [`update_write_status`](Self::update_write_status). Their return value tells
/// the task whether to continue, pause until notified, or stop.
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

    /// Marks backend transport teardown as complete.
    pub fn stopped(&self, e: Option<io::Error>) {
        self.st().stop_connection(e);
    }

    /// Checks if the I/O stream is stopped.
    pub fn is_stopped(&self) -> bool {
        self.st().flags.is_closed()
    }

    /// Takes a buffer for the next transport read.
    ///
    /// The returned buffer must be passed back exactly once through
    /// [`update_read_status`](Self::update_read_status), even when the read
    /// fails or would otherwise stop the task.
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

    /// Returns a transport read buffer and reports the read result.
    ///
    /// `Poll::Ready(Ok(n))` reports that `n` bytes were appended to `buf`.
    /// Zero marks the transport read side as closed and invokes the read filter
    /// chain once with no new bytes. This lets filters emit final buffered data
    /// or report truncated input. Further transport reads are parked, but
    /// buffered input remains decodable and the write side remains usable until
    /// graceful shutdown. `Poll::Ready(Err(_))` terminates the connection.
    /// `Poll::Pending` returns the buffer after a nonblocking operation made no
    /// progress or a submitted operation was canceled for reissue.
    ///
    /// The returned [`IoTaskStatus`] instructs the read task to continue
    /// immediately, pause until notified, or stop.
    pub fn update_read_status(
        &self,
        buf: BytesMut,
        status: Poll<io::Result<usize>>,
    ) -> IoTaskStatus {
        let st = self.st();
        let orig = st.buffer.read_dst_size();

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: read-status == {status:?} orig:{orig:?} flags:{:?}",
            st.tag(),
            st.flags
        );

        // release read buffer
        st.buffer.set_read_buf(buf, self.0.cfg());

        // process read buf
        let result = match status {
            Poll::Pending => Ok(()),
            Poll::Ready(status) => status.and_then(|nbytes| {
                if nbytes == 0 {
                    st.flags.set_read_eof();
                    st.wake_dispatch_task();
                }

                st.buffer
                    .process_read_buf(&self.0, nbytes)
                    .and_then(|status| {
                        let size = st.buffer.read_dst_size();

                        // The destination read buffer has new data, wake up the dispatcher
                        if size > orig {
                            if st.is_rd_backpressure_needed(size) {
                                log::trace!("{}: Read buf({size}), enable back-pressure", st.tag());
                                st.flags.set_read_ready_and_backpressure();
                            } else {
                                st.flags.set_read_ready();
                            }
                            #[cfg(feature = "trace")]
                            log::trace!("{}: New {size} bytes available", st.tag());
                            st.wake_dispatch_task();
                        }

                        if st.flags.is_read_notify() {
                            // If the "notify" flag is set, we must wake the
                            // dispatcher task whenever data is read from the source.
                            st.wake_dispatch_task();
                            st.flags.set_read_notifed();
                        }

                        // Check if the filter wrote data during buffer processing
                        if status.wants_write {
                            st.buffer.process_write_buf_force(&self.0)?;
                            self.0.consolidate_write_state(false)?;
                        }

                        // Check whether the filter notifies about readiness changes
                        if status.notify {
                            self.0.call_notify();
                        }
                        Ok(())
                    })
            }),
        };

        if let Err(err) = result {
            st.terminate_connection(Some(err));
            IoTaskStatus::Stop
        } else if st.flags.is_closed() {
            IoTaskStatus::Stop
        } else if st.flags.is_read_eof() || st.flags.is_read_paused_or_backpressure() {
            IoTaskStatus::Pause
        } else {
            IoTaskStatus::Io
        }
    }

    /// Provides access to bytes ready for the transport to write.
    ///
    /// Pending filter output is processed before `f` is invoked. The transport
    /// should remove only bytes it successfully writes and then report the
    /// result with [`update_write_status`](Self::update_write_status).
    pub fn with_write_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        // Write buffer processing may be delayed
        if let Err(e) = self.st().buffer.process_write_buf(&self.0) {
            self.st().terminate_connection(Some(e));
        }

        self.st().buffer.with_write_dst(|buffer| f(buffer))
    }

    /// Updates the write status.
    ///
    /// `Ok(true)` indicates that one or more bytes were successfully written
    /// to the transport; `Ok(false)` indicates no write progress. An error
    /// terminates the connection. The returned [`IoTaskStatus`] instructs the
    /// write task to continue immediately, pause until notified, or stop.
    pub fn update_write_status(&self, status: io::Result<bool>) -> IoTaskStatus {
        let st = &self.st();

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: write-status == {status:?} buf:{} flags:{:?}",
            st.tag(),
            st.buffer.write_buf_size(),
            st.flags
        );

        match status {
            Ok(written) => {
                let len = st.buffer.write_buf_size();
                // Full flush is active
                if st.flags.is_write_flush() {
                    // The write buffer must be fully written
                    if len == 0 {
                        st.wake_dispatch_task();
                    }
                } else if st.flags.is_wr_backpressure() && st.should_disable_wr_backpressure(len) {
                    // Write backpressure is active and write buffer is below threshold
                    st.wake_dispatch_task();
                }

                // Write notify is enabled
                if written && st.flags.is_write_notify() {
                    st.flags.unset_write_notify();
                    st.wake_read_task();
                    st.wake_write_task();
                }

                if st.flags.is_closed() {
                    IoTaskStatus::Stop
                } else if len == 0 {
                    // All data has been written, pause the write task.
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
            Err(err) => {
                st.terminate_connection(Some(err));
                IoTaskStatus::Stop
            }
        }
    }

    /// Polls transport-task shutdown state.
    ///
    /// If `flush` is `true`, this first waits until the write task is paused,
    /// indicating that currently buffered output has been handled. It then
    /// waits for the connection to close. The context's waker is registered
    /// while pending.
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

        // process filter shutdown
        let ready = match st.buffer.process_shutdown(&self.0) {
            Ok(Poll::Ready(())) => true,
            Ok(Poll::Pending) => false,
            Err(err) => {
                st.terminate_connection(Some(err));
                return;
            }
        };
        if self.0.consolidate_write_state(true).is_err() {
            return;
        }

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: shutdown filters, done:{ready:?} wr-buf:{:?}, flags:{:?}",
            st.tag(),
            st.buffer.write_buf_size(),
            st.flags,
        );

        // filters are shutdown and write task is paused
        if ready && st.flags.is_write_paused() && !st.flags.is_wr_send_scheduled() {
            st.filters_stopped();
        } else if !ready && (st.flags.is_read_paused() || st.flags.is_read_ready_and_backpressure())
        {
            // if read buffer is not consumed it is unlikely
            // that filter will properly complete shutdown
            st.set_shutdown_error(io::Error::other(
                "filter shutdown blocked by unread buffered data",
            ));
            st.filters_stopped();
        } else if st.cfg.disconnect_timeout().non_zero() {
            // filter shutdown timeout
            let timeout = st
                .shutdown_timeout
                .take()
                .unwrap_or_else(|| sleep(st.cfg.disconnect_timeout()));
            if timeout.poll_elapsed(cx).is_ready() {
                st.set_shutdown_error(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "filter shutdown timed out",
                ));
                st.filters_stopped();
            } else {
                st.shutdown_timeout.set(Some(timeout));
            }
        }
    }

    /// Notifies read tasks.
    pub fn notify(&self) {
        self.0.0.wake_read_task();
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
    use crate::{FilterBuf, FilterLayer, Io, testing::IoTest};
    use ntex_util::future::lazy;

    #[ntex::test]
    async fn ctx_basics() {
        let (_, server) = IoTest::create();

        let state = Io::from(server);
        let ctx = IoContext::new(state.get_ref());
        let _ = ctx.flags();
        assert_ne!(ctx.id(), Id::default());
        assert!(format!("{ctx:?}").contains("IoContext"));
    }

    #[ntex::test]
    async fn pending_read_completion_is_not_eof() {
        let (_, server) = IoTest::create();
        let state = Io::from(server);
        let ctx = IoContext::new(state.get_ref());

        assert!(lazy(|cx| state.poll_read_more(cx)).await.is_pending());
        assert_ne!(
            ctx.update_read_status(ctx.get_read_buf(), Poll::Pending),
            IoTaskStatus::Stop
        );
        assert!(!ctx.is_stopped());
        assert!(lazy(|cx| state.poll_read_more(cx)).await.is_pending());

        assert_eq!(
            ctx.update_read_status(ctx.get_read_buf(), Poll::Ready(Ok(0))),
            IoTaskStatus::Pause
        );
        assert!(!ctx.is_stopped());
        assert!(matches!(
            lazy(|cx| state.poll_read_more(cx)).await,
            Poll::Ready(Ok(None))
        ));
    }

    #[derive(Debug)]
    struct FinishOnEof;

    impl FilterLayer for FinishOnEof {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            if buf.io().is_read_eof() {
                buf.with_read_buffers(|_, dst| dst.extend_from_slice(b"final"));
            }
            Ok(())
        }

        fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }
    }

    #[ntex::test]
    async fn clean_eof_is_processed_by_filters() {
        let (_, server) = IoTest::create();
        let state = Io::from(server).add_filter(FinishOnEof);
        let ctx = IoContext::new(state.get_ref());

        assert!(lazy(|cx| state.poll_read_notify(cx)).await.is_pending());
        assert_eq!(
            ctx.update_read_status(ctx.get_read_buf(), Poll::Ready(Ok(0))),
            IoTaskStatus::Pause
        );
        assert!(state.is_read_eof());
        assert!(matches!(
            lazy(|cx| state.poll_read_notify(cx)).await,
            Poll::Ready(Ok(Some(())))
        ));
        assert!(matches!(
            lazy(|cx| state.poll_read_notify(cx)).await,
            Poll::Ready(Ok(None))
        ));
        assert_eq!(state.with_read_buf(BytesMut::take), b"final");
        assert!(matches!(
            lazy(|cx| state.poll_read_more(cx)).await,
            Poll::Ready(Ok(None))
        ));
    }

    #[derive(Debug)]
    struct RejectEof;

    impl FilterLayer for RejectEof {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            if buf.io().is_read_eof() {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated filtered stream",
                ))
            } else {
                Ok(())
            }
        }

        fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
            Ok(())
        }
    }

    #[ntex::test]
    async fn clean_eof_filter_error_terminates_connection() {
        let (_, server) = IoTest::create();
        let state = Io::from(server).add_filter(RejectEof);
        let ctx = IoContext::new(state.get_ref());

        assert_eq!(
            ctx.update_read_status(ctx.get_read_buf(), Poll::Ready(Ok(0))),
            IoTaskStatus::Stop
        );
        assert!(state.is_read_eof());
        assert!(state.is_terminating());
    }
}
