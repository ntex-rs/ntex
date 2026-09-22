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
///
/// # Shutdown
///
/// A transport task runs until [`poll_read_ready`](Self::poll_read_ready) or
/// [`poll_write_ready`](Self::poll_write_ready) reports [`Readiness::Close`],
/// or until a status update returns [`IoTaskStatus::Stop`]. All of those imply
/// that the connection is already closing or closed.
///
/// Graceful shutdown runs in two phases. In the first the filters shut down
/// while both directions stay open. In the second, buffered output is drained
/// into the transport and incoming data is read and discarded;
/// [`Readiness::Close`] is reported only once nothing is left to write. A
/// single disconnect timeout bounds both phases, and terminates the connection
/// if it elapses.
///
/// So by the time the loop exits there is nothing left to drain, whether the
/// connection was shut down gracefully or terminated. A task must never attempt
/// a final flush on the way out; it should close both directions of the
/// transport immediately and report the outcome through
/// [`stopped`](Self::stopped).
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
    ///
    /// Resolves to [`Readiness::Ready`] or [`Readiness::Close`], or stays
    /// `Pending`. Reads continue throughout a graceful shutdown, first so that
    /// filters can complete theirs, then to drain and discard whatever the peer
    /// still sends, so `Close` is resolved here only once the connection is
    /// terminated.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.shutdown_filters(cx);
        self.0.filter().poll_read_ready(cx)
    }

    #[inline]
    /// Checks readiness for write operations.
    ///
    /// Resolves to [`Readiness::Ready`] or [`Readiness::Close`], or stays
    /// `Pending`. Unlike the read path this reports `Close` at the end of a
    /// graceful shutdown as well, once buffered output has been drained, so the
    /// task must not flush again.
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.poll_shutdown_deadline(cx);
        self.0.filter().poll_write_ready(cx)
    }

    /// Force-terminates the I/O stream.
    ///
    /// This is the immediate path, not a graceful shutdown: pending
    /// application work is not drained. Call
    /// [`stopped`](Self::stopped) afterwards, once transport teardown has
    /// actually finished.
    pub fn stop(&self, e: Option<io::Error>) {
        self.st().terminate_connection(e);
    }

    /// Marks backend transport teardown as complete.
    pub fn stopped(&self, e: Option<io::Error>) {
        self.st().stop_connection(e);
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

        // Transport shutdown phase, the filters are shut down and the
        // connection is about to be closed. Input is drained and discarded so
        // that the socket receive queue is empty when it is closed.
        if st.flags.is_stopping() {
            let mut buf = buf;
            buf.clear();
            st.buffer.set_read_buf(buf, self.0.cfg());

            return match status {
                Poll::Ready(Ok(n)) if n != 0 => IoTaskStatus::Io,
                // A zero-length read is a clean eof; an error means the peer is
                // gone. Either way there is nothing left to drain. Neither
                // terminates the connection: the write side keeps draining
                // until it completes or the shutdown deadline elapses.
                Poll::Ready(_) => {
                    st.flags.set_read_eof();
                    IoTaskStatus::Pause
                }
                Poll::Pending => IoTaskStatus::Pause,
            };
        }

        // release read buffer
        st.buffer.set_read_buf(buf, self.0.cfg());

        // process read buf
        let result = match status {
            Poll::Pending => Ok(()),
            Poll::Ready(status) => status.and_then(|nbytes| {
                if nbytes == 0 {
                    if st.flags.is_read_eof() {
                        // A clean eof is reported to the filter chain exactly
                        // once, no matter how often the transport reports it.
                        return Ok(());
                    }
                    st.flags.set_read_eof();
                    st.wake_dispatch_task();
                }

                st.buffer.process_read_buf(&self.0).and_then(|status| {
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
            // A read failure while the filters are shutting down does not
            // terminate the connection: the filter handshake cannot complete,
            // but buffered output is still drained by the transport shutdown
            // phase.
            if st.flags.is_stopping_filters() {
                Self::stop_filters(st, Some(err));
                IoTaskStatus::Pause
            } else {
                st.terminate_connection(Some(err));
                IoTaskStatus::Stop
            }
        } else if st.flags.is_aborted() {
            IoTaskStatus::Stop
        } else if st.flags.is_read_eof() || st.flags.is_read_paused_or_backpressure() {
            IoTaskStatus::Pause
        } else {
            IoTaskStatus::Io
        }
    }

    /// Provides access to the transport-facing write destination.
    ///
    /// This holds the encoded bytes that are ready to be written out.
    ///
    /// Pending filter output is processed before `f` is invoked. The transport
    /// may write bytes out directly, or take ownership of pages and write them
    /// later; any page it removes is counted as in-flight output until it is
    /// either returned to this buffer or reported as written through
    /// [`update_write_status`](Self::update_write_status).
    pub fn with_write_dst<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        let st = self.st();

        // Write buffer processing may be delayed
        if let Err(e) = st.buffer.process_write_buf(&self.0) {
            st.terminate_connection(Some(e));
        }

        let before = st.buffer.write_buf_size();
        let result = st.buffer.with_write_dst(|buffer| f(buffer));
        st.track_wr_inflight(before, st.buffer.write_buf_size());

        result
    }

    /// Updates the write status.
    ///
    /// `Ok(n)` reports that the write attempt completed without error and that
    /// `n` bytes reached the peer; `n` is zero when the attempt moved nothing.
    /// Any page the transport is still holding stays counted as outstanding
    /// output, so it must either be returned to the write buffer or reported
    /// here. An error terminates the connection. The returned
    /// [`IoTaskStatus`] instructs the write task to continue, pause until
    /// notified, or stop.
    pub fn update_write_status(&self, status: io::Result<usize>) -> IoTaskStatus {
        let st = &self.st();

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: write-status == {status:?} buf:{} inflight:{} flags:{:?}",
            st.tag(),
            st.buffer.write_buf_size(),
            st.wr_inflight.get(),
            st.flags
        );

        match status {
            Ok(written) => {
                st.wr_inflight_written(written);

                let len = st.buffer.write_buf_size();
                let outstanding = st.write_outstanding();

                // Full flush is active
                if st.flags.is_write_flush() {
                    // All output must reach the peer, including in-flight pages
                    if outstanding == 0 {
                        st.wake_dispatch_task();
                    }
                } else if st.flags.is_wr_backpressure()
                    && st.should_disable_wr_backpressure(outstanding)
                {
                    // Write backpressure is active and outstanding output is
                    // below the threshold
                    st.wake_dispatch_task();
                }

                if st.flags.is_aborted() {
                    IoTaskStatus::Stop
                } else if len == 0 {
                    // Nothing left to submit, pause the write task. In-flight
                    // pages are not actionable here, their completion wakes
                    // the task again.
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

        // all pending output has reached the transport
        let flushed = st.flags.is_write_paused() && !st.flags.is_wr_send_scheduled();

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: shutdown filters, done:{ready:?} flushed:{flushed:?} wr-buf:{:?}, flags:{:?}",
            st.tag(),
            st.buffer.write_buf_size(),
            st.flags,
        );

        // filters are shutdown and write task is paused
        if ready && flushed {
            st.filters_stopped();
            return;
        }

        // After a clean read EOF no further input can arrive, so a filter that
        // is waiting for the peer can never finish. The peer closing first is
        // a normal close, so this is not reported as an error.
        let eof = !ready && st.flags.is_read_eof();

        // If the read buffer is not consumed it is unlikely that the filter
        // will ever complete its shutdown.
        let blocked = !ready
            && !eof
            && (st.flags.is_read_paused() || st.flags.is_read_ready_and_backpressure());

        // The filter shutdown cannot complete. Move on to the transport
        // shutdown phase, which drains whatever output has been produced so far
        // and then closes the connection.
        if eof || blocked {
            if eof {
                log::debug!("{}: Peer closed before filter shutdown completed", st.tag());
            }
            Self::stop_filters(st, blocked.then(blocked_err));
            return;
        }

        if st.cfg.disconnect_timeout().non_zero() {
            // filter shutdown timeout
            let timeout = st
                .shutdown_timeout
                .take()
                .unwrap_or_else(|| sleep(st.cfg.disconnect_timeout()));
            if timeout.poll_elapsed(cx).is_ready() {
                Self::stop_filters(
                    st,
                    Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "filter shutdown timed out",
                    )),
                );
            }
            // the deadline is put back even once it has elapsed, so that the
            // transport shutdown phase sees it expired instead of starting a
            // second one
            st.shutdown_timeout.set(Some(timeout));
        }
    }

    /// Polls the shutdown deadline during the transport shutdown phase.
    ///
    /// The deadline is created when filter shutdown starts and is not reset
    /// here, so a single `disconnect_timeout` bounds both shutdown phases. When
    /// it elapses the connection is terminated and any output that has not
    /// reached the transport is lost.
    fn poll_shutdown_deadline(&self, cx: &mut Context<'_>) {
        let st = &self.st();
        if !st.flags.is_stopping() || !st.cfg.disconnect_timeout().non_zero() {
            return;
        }

        // Nothing is left to drain, so the connection closes cleanly on the
        // next readiness check and the deadline does not apply. Without this
        // the shutdown would be reported as timed out whenever the filter
        // phase happened to consume the whole deadline.
        if st.write_outstanding() == 0 {
            return;
        }

        let timeout = st
            .shutdown_timeout
            .take()
            .unwrap_or_else(|| sleep(st.cfg.disconnect_timeout()));
        if timeout.poll_elapsed(cx).is_ready() {
            let len = st.write_outstanding();
            if len != 0 {
                log::warn!(
                    "{}: Shutdown timed out, discarding {len} bytes of buffered output",
                    st.tag()
                );
            }
            st.terminate_connection(Some(io::Error::new(
                io::ErrorKind::TimedOut,
                "io shutdown timed out",
            )));
        } else {
            st.shutdown_timeout.set(Some(timeout));
        }
    }

    /// Leaves the filter shutdown phase after an incomplete shutdown.
    ///
    /// Output that has not reached the transport is not lost: the transport
    /// shutdown phase drains it before closing the connection.
    ///
    /// The error is recorded here rather than when the failure is first
    /// detected: while an error is set, `IoRef::consolidate_write_state()`
    /// short-circuits, which would stop the write buffer from draining.
    fn stop_filters(st: &IoState, err: Option<io::Error>) {
        if let Some(err) = err {
            st.set_shutdown_error(err);
        }
        st.filters_stopped();
    }

    /// Notifies read tasks.
    pub fn notify(&self) {
        self.0.0.wake_read_task();
    }
}

fn blocked_err() -> io::Error {
    io::Error::other("filter shutdown blocked by unread buffered data")
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
        assert!(lazy(|cx| state.poll_read_more(cx)).await.is_pending());

        assert_eq!(
            ctx.update_read_status(ctx.get_read_buf(), Poll::Ready(Ok(0))),
            IoTaskStatus::Pause
        );
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
    async fn eof_reports_available_input_once() {
        let (_, server) = IoTest::create();
        let state = Io::from(server);
        let ctx = IoContext::new(state.get_ref());

        // data arrives, then a clean eof
        ctx.update_read_status(BytesMut::copy_from_slice(b"12345"), Poll::Ready(Ok(5)));
        ctx.update_read_status(ctx.get_read_buf(), Poll::Ready(Ok(0)));

        // the buffered input is reported once
        assert!(matches!(
            lazy(|cx| state.poll_read_more(cx)).await,
            Poll::Ready(Ok(Some(())))
        ));

        // accessing the buffer marks the input as reported
        assert_eq!(state.with_read_dst(|b| b.len()), 5);
        assert!(matches!(
            lazy(|cx| state.poll_read_more(cx)).await,
            Poll::Ready(Ok(None))
        ));

        // "no further input" does not mean the read buffer is empty, the
        // remaining bytes are still decodable
        assert_eq!(state.with_read_dst(BytesMut::take), b"12345");
    }

    #[ntex::test]
    async fn shutdown_keeps_unconsumed_input_visible() {
        let (_, server) = IoTest::create();
        let state = Io::from(server);
        let ctx = IoContext::new(state.get_ref());

        // input arrives but the dispatcher has not consumed it yet
        ctx.update_read_status(BytesMut::copy_from_slice(b"12345"), Poll::Ready(Ok(5)));
        assert!(ctx.flags().is_read_ready());

        // starting a shutdown must not discard the "input available" signal
        assert!(lazy(|cx| state.poll_shutdown(cx)).await.is_pending());
        assert!(ctx.flags().is_read_ready());

        // so the read task is handed a fresh buffer instead of the one the
        // dispatcher still has to decode
        assert!(ctx.get_read_buf().is_empty());
        assert_eq!(state.with_read_dst(BytesMut::take), b"12345");
    }

    #[ntex::test]
    async fn clean_eof_is_processed_by_filters_once() {
        let (_, server) = IoTest::create();
        let state = Io::from(server).add_filter(FinishOnEof);
        let ctx = IoContext::new(state.get_ref());

        for _ in 0..3 {
            assert_eq!(
                ctx.update_read_status(ctx.get_read_buf(), Poll::Ready(Ok(0))),
                IoTaskStatus::Pause
            );
            assert!(state.is_read_eof());
        }
        assert_eq!(state.with_read_dst(BytesMut::take), b"final");
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
        assert_eq!(state.with_read_dst(BytesMut::take), b"final");
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
