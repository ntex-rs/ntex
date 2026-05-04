use std::{fmt, io, ptr, task::Context, task::Poll};

use ntex_bytes::BytePages;
use ntex_util::time::sleep;

use crate::{Buffer, Flags, IoRef, IoTaskStatus, Readiness};

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

    #[doc(hidden)]
    #[inline]
    pub fn id(&self) -> usize {
        ptr::from_ref(self.0.0.as_ref()) as usize
    }

    #[inline]
    /// Io tag
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[inline]
    #[doc(hidden)]
    /// Io flags
    pub fn flags(&self) -> crate::flags::Flags {
        self.0.flags()
    }

    #[inline]
    /// Check readiness for read operations
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.shutdown_filters(cx);
        self.0.filter().poll_read_ready(cx)
    }

    #[inline]
    /// Check readiness for write operations
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.0.filter().poll_write_ready(cx)
    }

    #[inline]
    /// Stop io
    pub fn stop(&self, e: Option<io::Error>) {
        self.0.0.io_stopped(e);
    }

    #[inline]
    /// Check if Io stopped
    pub fn is_stopped(&self) -> bool {
        self.0.flags().is_stopped()
    }

    /// Wait when io get closed or preparing for close
    pub fn shutdown(&self, flush: bool, cx: &mut Context<'_>) -> Poll<()> {
        let st = &self.0.0;
        let flags = self.0.flags();

        if flush && !flags.contains(Flags::IO_STOPPED) {
            if flags.write_paused() {
                return Poll::Ready(());
            }
            st.insert_flags(Flags::WR_TASK_WAIT);
            st.write_task.register(cx.waker());
            Poll::Pending
        } else if !flags.intersects(Flags::IO_STOPPING | Flags::IO_STOPPED) {
            st.write_task.register(cx.waker());
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    /// Get read buffer
    pub fn get_read_buf(&self) -> Buffer {
        let inner = &self.0.0;

        let buf = if inner.flags.get().is_read_buf_ready() {
            // read buffer is still not read by dispatcher
            // we cannot touch it
            inner.read_buf().get()
        } else {
            inner
                .buffer
                .get_read_buf()
                .unwrap_or_else(|| inner.read_buf().get())
        };

        Buffer::new(buf)
    }

    /// Resize read buffer
    pub fn resize_read_buf(&self, buf: &mut Buffer) {
        self.0.0.read_buf().resize(&mut buf.buf);
    }

    /// Set read buffer
    pub fn release_read_buf(
        &self,
        buffer: Buffer,
        result: Poll<Result<(), Option<io::Error>>>,
    ) -> IoTaskStatus {
        let inner = &self.0.0;
        let hw = self.0.cfg().read_buf().high;
        let nbytes = buffer.has_newbytes();
        let read_buf = inner.buffer.read_destination_size();

        let mut full = false;

        let st_res = if nbytes {
            // handle buffer changes
            match inner.buffer.process_read_buf(&self.0, buffer.buf) {
                Ok(()) => {
                    let buf_size = inner.buffer.read_destination_size();

                    if buf_size > read_buf {
                        // dest buffer has new data, wake up dispatcher
                        if buf_size >= hw {
                            log::trace!(
                                "{}: Io read buffer is too large {}, enable read back-pressure",
                                self.tag(),
                                buf_size
                            );
                            full = true;
                            inner.insert_flags(Flags::BUF_R_READY | Flags::BUF_R_FULL);
                        } else {
                            inner.insert_flags(Flags::BUF_R_READY);
                        }
                        log::trace!(
                            "{}: New {} bytes available, wakeup dispatcher",
                            self.tag(),
                            buf_size
                        );
                        inner.dispatch_task.wake();
                    } else {
                        if buf_size >= hw {
                            // read task is paused because of read back-pressure
                            // but there is no new data in top most read buffer
                            // so we need to wake up read task to read more data
                            // otherwise read task would sleep forever
                            full = true;
                            inner.remove_flags(Flags::BUF_R_FULL);
                        }
                        if inner.flags.get().is_waiting_for_read() {
                            // in case of "notify" we must wake up dispatch task
                            // if we read any data from source
                            inner.dispatch_task.wake();
                        }
                    }

                    // while reading, filter wrote some data
                    // in that case filters need to process write buffers
                    // and potentialy wake write task
                    if self.0.flags().is_want_to_write() {
                        inner.remove_flags(Flags::IO_WANT_WRITE);
                        inner.buffer.process_write_buf_force(&self.0)
                    } else {
                        Ok(())
                    }
                }
                Err(err) => Err(err),
            }
        } else {
            inner.buffer.set_read_buf(&self.0, buffer.buf);
            Ok(())
        };

        match result {
            Poll::Ready(Ok(())) => {
                if let Err(e) = st_res {
                    inner.io_stopped(Some(e));
                    IoTaskStatus::Stop
                } else if !nbytes {
                    inner.io_stopped(None);
                    IoTaskStatus::Stop
                } else if full {
                    IoTaskStatus::Pause
                } else {
                    IoTaskStatus::Io
                }
            }
            Poll::Ready(Err(e)) => {
                inner.io_stopped(e);
                IoTaskStatus::Stop
            }
            Poll::Pending => {
                if let Err(e) = st_res {
                    inner.io_stopped(Some(e));
                    IoTaskStatus::Stop
                } else if full {
                    IoTaskStatus::Pause
                } else {
                    IoTaskStatus::Io
                }
            }
        }
    }

    #[inline]
    /// Get write buffer
    pub fn with_write_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        // write buffer processing could be delayed,
        // need to call filter chain for processing
        if let Err(e) = self.0.0.buffer.process_write_buf(&self.0) {
            self.0.0.io_stopped(Some(e));
        }

        // access to output write buffer
        self.0.0.buffer.with_write_destination(|buffer| f(buffer))
    }

    /// Set write buffer
    pub fn update_write_buf(&self, result: Poll<io::Result<()>>) -> IoTaskStatus {
        let inner = &self.0.0;

        match result {
            Poll::Pending => {
                let len = inner.buffer.write_destination_size();

                // check write backpressure
                if inner.flags.get().contains(Flags::BUF_W_BACKPRESSURE)
                    && len < inner.write_buf().half
                {
                    inner.remove_flags(Flags::BUF_W_BACKPRESSURE);
                    inner.dispatch_task.wake();
                }
                IoTaskStatus::Pause
            }
            Poll::Ready(Ok(())) => {
                let mut flags = inner.flags.get();
                let len = inner.buffer.write_destination_size();

                // check write backpressure
                if flags.contains(Flags::BUF_W_BACKPRESSURE) && len < inner.write_buf().half
                {
                    flags.remove(Flags::BUF_W_BACKPRESSURE);
                    inner.dispatch_task.wake();
                }

                if flags.is_task_waiting_for_write() {
                    flags.task_waiting_for_write_is_done();
                    inner.write_task.wake();
                }

                if flags.is_waiting_for_write() {
                    flags.waiting_for_write_is_done();
                    inner.dispatch_task.wake();
                }

                let result = if self.is_stopped() {
                    IoTaskStatus::Stop
                } else if len == 0 && inner.buffer.write_source_size() == 0 {
                    // all data has been written
                    flags.insert(Flags::WR_PAUSED);

                    IoTaskStatus::Pause
                } else {
                    IoTaskStatus::Io
                };

                inner.flags.set(flags);
                result
            }
            Poll::Ready(Err(e)) => {
                inner.io_stopped(Some(e));
                IoTaskStatus::Stop
            }
        }
    }

    fn shutdown_filters(&self, cx: &mut Context<'_>) {
        let io = &self.0;
        let st = &self.0.0;

        if st.flags.get().is_shutting_down_filters() {
            match st.buffer.process_shutdown(io) {
                Ok(Poll::Ready(())) => {
                    st.io_stopping();
                }
                Ok(Poll::Pending) => {
                    // check read buffer, if buffer is not consumed it is unlikely
                    // that filter will properly complete shutdown
                    let flags = st.flags.get();
                    if flags.contains(Flags::RD_PAUSED)
                        || flags.contains(Flags::BUF_R_FULL | Flags::BUF_R_READY)
                    {
                        st.io_stopping();
                    } else {
                        // filter shutdown timeout
                        let timeout = st
                            .shutdown_timeout
                            .take()
                            .unwrap_or_else(|| sleep(io.cfg().disconnect_timeout()));
                        if timeout.poll_elapsed(cx).is_ready() {
                            st.io_stopping();
                        } else {
                            st.shutdown_timeout.set(Some(timeout));
                        }
                    }
                    if let Err(err) = st.buffer.process_write_buf(&self.0) {
                        st.io_stopped(Some(err));
                    }
                }
                Err(err) => {
                    st.io_stopped(Some(err));
                }
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
        assert!(ctx.id() != 0);
        assert!(format!("{ctx:?}").contains("IoContext"));
    }
}
