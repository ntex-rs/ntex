use std::{cell::Cell, fmt, io, task::Context, task::Poll};

use ntex_bytes::{Buf, BytesVec};
use ntex_util::time::{sleep, Sleep};

use crate::{FilterCtx, Flags, IoRef, IoTaskStatus, Readiness};

/// Context for io read task
pub struct IoContext(IoRef, Cell<Option<Sleep>>);

impl fmt::Debug for IoContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoContext").field("io", &self.0).finish()
    }
}

impl IoContext {
    pub(crate) fn new(io: &IoRef) -> Self {
        Self(io.clone(), Cell::new(None))
    }

    #[doc(hidden)]
    #[inline]
    pub fn id(&self) -> usize {
        self.0 .0.as_ref() as *const _ as usize
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
        self.0 .0.io_stopped(e);
    }

    #[inline]
    /// Check if Io stopped
    pub fn is_stopped(&self) -> bool {
        self.0.flags().is_stopped()
    }

    /// Wait when io get closed or preparing for close
    pub fn shutdown(&self, flush: bool, cx: &mut Context<'_>) -> Poll<()> {
        let st = &self.0 .0;
        let flags = self.0.flags();

        if flush && !flags.contains(Flags::IO_STOPPED) {
            if flags.intersects(Flags::WR_PAUSED | Flags::IO_STOPPED) {
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
    pub fn get_read_buf(&self) -> BytesVec {
        let inner = &self.0 .0;

        if inner.flags.get().is_read_buf_ready() {
            // read buffer is still not read by dispatcher
            // we cannot touch it
            inner.read_buf().get()
        } else {
            inner
                .buffer
                .get_read_source()
                .unwrap_or_else(|| inner.read_buf().get())
        }
    }

    /// Resize read buffer
    pub fn resize_read_buf(&self, buf: &mut BytesVec) {
        self.0 .0.read_buf().resize(buf);
    }

    /// Set read buffer
    pub fn release_read_buf(
        &self,
        nbytes: usize,
        buf: BytesVec,
        result: Poll<Result<(), Option<io::Error>>>,
    ) -> IoTaskStatus {
        let inner = &self.0 .0;
        let orig_size = inner.buffer.read_destination_size();
        let hw = self.0.cfg().read_buf().high;

        if let Some(mut first_buf) = inner.buffer.get_read_source() {
            first_buf.extend_from_slice(&buf);
            inner.buffer.set_read_source(&self.0, first_buf);
        } else {
            inner.buffer.set_read_source(&self.0, buf);
        }

        let mut full = false;

        // handle buffer changes
        let st_res = if nbytes > 0 {
            match self
                .0
                .filter()
                .process_read_buf(FilterCtx::new(&self.0, &inner.buffer), nbytes)
            {
                Ok(status) => {
                    let buffer_size = inner.buffer.read_destination_size();
                    if buffer_size.saturating_sub(orig_size) > 0 {
                        // dest buffer has new data, wake up dispatcher
                        if buffer_size >= hw {
                            log::trace!(
                                "{}: Io read buffer is too large {}, enable read back-pressure",
                                self.tag(),
                                buffer_size
                            );
                            full = true;
                            inner.insert_flags(Flags::BUF_R_READY | Flags::BUF_R_FULL);
                        } else {
                            inner.insert_flags(Flags::BUF_R_READY);
                        }
                        log::trace!(
                            "{}: New {} bytes available, wakeup dispatcher",
                            self.tag(),
                            buffer_size
                        );
                        inner.dispatch_task.wake();
                    } else {
                        if buffer_size >= hw {
                            // read task is paused because of read back-pressure
                            // but there is no new data in top most read buffer
                            // so we need to wake up read task to read more data
                            // otherwise read task would sleep forever
                            full = true;
                            inner.read_task.wake();
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
                    if status.need_write {
                        self.0
                            .filter()
                            .process_write_buf(FilterCtx::new(&self.0, &inner.buffer))
                    } else {
                        Ok(())
                    }
                }
                Err(err) => Err(err),
            }
        } else {
            Ok(())
        };

        match result {
            Poll::Ready(Ok(_)) => {
                if let Err(e) = st_res {
                    inner.io_stopped(Some(e));
                    IoTaskStatus::Stop
                } else if nbytes == 0 {
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
    pub fn get_write_buf(&self) -> Option<BytesVec> {
        self.0 .0.buffer.get_write_destination().and_then(|buf| {
            if buf.is_empty() {
                None
            } else {
                Some(buf)
            }
        })
    }

    /// Set write buffer
    pub fn release_write_buf(
        &self,
        mut buf: BytesVec,
        result: Poll<io::Result<usize>>,
    ) -> IoTaskStatus {
        let result = match result {
            Poll::Ready(Ok(0)) => {
                log::trace!("{}: Disconnected during flush", self.tag());
                Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write frame to transport",
                ))
            }
            Poll::Ready(Ok(n)) => {
                if n == buf.len() {
                    buf.clear();
                    Ok(0)
                } else {
                    buf.advance(n);
                    Ok(buf.len())
                }
            }
            Poll::Ready(Err(e)) => Err(e),
            Poll::Pending => Ok(buf.len()),
        };

        let inner = &self.0 .0;

        // set buffer back
        let result = match result {
            Ok(0) => {
                self.0.cfg().write_buf().release(buf);
                Ok(inner.buffer.write_destination_size())
            }
            Ok(_) => {
                if let Some(b) = inner.buffer.get_write_destination() {
                    buf.extend_from_slice(&b);
                    self.0.cfg().write_buf().release(b);
                }
                let l = buf.len();
                inner.buffer.set_write_destination(buf);
                Ok(l)
            }
            Err(e) => Err(e),
        };

        match result {
            Ok(0) => {
                let mut flags = inner.flags.get();

                // all data has been written
                flags.insert(Flags::WR_PAUSED);

                if flags.is_task_waiting_for_write() {
                    flags.task_waiting_for_write_is_done();
                    inner.write_task.wake();
                }

                if flags.is_waiting_for_write() {
                    flags.waiting_for_write_is_done();
                    inner.dispatch_task.wake();
                }
                inner.flags.set(flags);
                if self.is_stopped() {
                    IoTaskStatus::Stop
                } else {
                    IoTaskStatus::Pause
                }
            }
            Ok(len) => {
                // if write buffer is smaller than high watermark value, turn off back-pressure
                if inner.flags.get().contains(Flags::BUF_W_BACKPRESSURE)
                    && len < inner.write_buf().half
                {
                    inner.remove_flags(Flags::BUF_W_BACKPRESSURE);
                    inner.dispatch_task.wake();
                }
                IoTaskStatus::Io
            }
            Err(e) => {
                inner.io_stopped(Some(e));
                IoTaskStatus::Stop
            }
        }
    }

    fn shutdown_filters(&self, cx: &mut Context<'_>) {
        let io = &self.0;
        let st = &self.0 .0;
        let flags = st.flags.get();
        if flags.contains(Flags::IO_STOPPING_FILTERS)
            && !flags.intersects(Flags::IO_STOPPED | Flags::IO_STOPPING)
        {
            match io.filter().shutdown(FilterCtx::new(io, &st.buffer)) {
                Ok(Poll::Ready(())) => {
                    st.dispatch_task.wake();
                    st.insert_flags(Flags::IO_STOPPING);
                }
                Ok(Poll::Pending) => {
                    // check read buffer, if buffer is not consumed it is unlikely
                    // that filter will properly complete shutdown
                    let flags = st.flags.get();
                    if flags.contains(Flags::RD_PAUSED)
                        || flags.contains(Flags::BUF_R_FULL | Flags::BUF_R_READY)
                    {
                        st.dispatch_task.wake();
                        st.insert_flags(Flags::IO_STOPPING);
                    } else {
                        // filter shutdown timeout
                        let timeout = self
                            .1
                            .take()
                            .unwrap_or_else(|| sleep(io.cfg().disconnect_timeout()));
                        if timeout.poll_elapsed(cx).is_ready() {
                            st.dispatch_task.wake();
                            st.insert_flags(Flags::IO_STOPPING);
                        } else {
                            self.1.set(Some(timeout));
                        }
                    }
                }
                Err(err) => {
                    st.io_stopped(Some(err));
                }
            }
            if let Err(err) = io
                .filter()
                .process_write_buf(FilterCtx::new(io, &st.buffer))
            {
                st.io_stopped(Some(err));
            }
        }
    }
}

impl Clone for IoContext {
    fn clone(&self) -> Self {
        Self(self.0.clone(), Cell::new(None))
    }
}
