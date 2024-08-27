use std::{future::poll_fn, future::Future, io, task::Context, task::Poll};

use ntex_bytes::{BufMut, BytesVec, PoolRef};
use ntex_util::task;

use crate::{Flags, IoRef, ReadStatus, WriteStatus};

#[derive(Debug)]
/// Context for io read task
pub struct ReadContext(IoRef);

impl ReadContext {
    pub(crate) fn new(io: &IoRef) -> Self {
        Self(io.clone())
    }

    #[inline]
    /// Io tag
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[inline]
    /// Check readiness for read operations
    pub async fn ready(&self) -> ReadStatus {
        poll_fn(|cx| self.0.filter().poll_read_ready(cx)).await
    }

    #[inline]
    /// Wait when io get closed or preparing for close
    pub async fn wait_for_close(&self) {
        poll_fn(|cx| {
            let flags = self.0.flags();

            if flags.intersects(Flags::IO_STOPPING | Flags::IO_STOPPED) {
                Poll::Ready(())
            } else {
                self.0 .0.read_task.register(cx.waker());
                if flags.contains(Flags::IO_STOPPING_FILTERS) {
                    shutdown_filters(&self.0);
                }
                Poll::Pending
            }
        })
        .await
    }

    #[inline]
    /// Check readiness for read operations
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        self.0.filter().poll_read_ready(cx)
    }

    /// Get read buffer
    pub fn with_buf<F>(&self, f: F) -> Poll<()>
    where
        F: FnOnce(&mut BytesVec, usize, usize) -> Poll<io::Result<()>>,
    {
        let inner = &self.0 .0;
        let (hw, lw) = self.0.memory_pool().read_params().unpack();
        let (result, nbytes, total) = inner.buffer.with_read_source(&self.0, |buf| {
            let total = buf.len();

            // call provided callback
            let result = f(buf, hw, lw);
            let total2 = buf.len();
            let nbytes = if total2 > total { total2 - total } else { 0 };
            (result, nbytes, total2)
        });

        // handle buffer changes
        if nbytes > 0 {
            let filter = self.0.filter();
            let _ = filter
                .process_read_buf(&self.0, &inner.buffer, 0, nbytes)
                .and_then(|status| {
                    if status.nbytes > 0 {
                        // dest buffer has new data, wake up dispatcher
                        if inner.buffer.read_destination_size() >= hw {
                            log::trace!(
                                "{}: Io read buffer is too large {}, enable read back-pressure",
                                self.0.tag(),
                                total
                            );
                            inner.insert_flags(Flags::BUF_R_READY | Flags::BUF_R_FULL);
                        } else {
                            inner.insert_flags(Flags::BUF_R_READY);

                            if nbytes >= hw {
                                // read task is paused because of read back-pressure
                                // but there is no new data in top most read buffer
                                // so we need to wake up read task to read more data
                                // otherwise read task would sleep forever
                                inner.read_task.wake();
                            }
                        }
                        log::trace!(
                            "{}: New {} bytes available, wakeup dispatcher",
                            self.0.tag(),
                            nbytes
                        );
                        inner.dispatch_task.wake();
                    } else {
                        if nbytes >= hw {
                            // read task is paused because of read back-pressure
                            // but there is no new data in top most read buffer
                            // so we need to wake up read task to read more data
                            // otherwise read task would sleep forever
                            inner.read_task.wake();
                        }
                        if inner.flags.get().contains(Flags::RD_NOTIFY) {
                            // in case of "notify" we must wake up dispatch task
                            // if we read any data from source
                            inner.dispatch_task.wake();
                        }
                    }

                    // while reading, filter wrote some data
                    // in that case filters need to process write buffers
                    // and potentialy wake write task
                    if status.need_write {
                        filter.process_write_buf(&self.0, &inner.buffer, 0)
                    } else {
                        Ok(())
                    }
                })
                .map_err(|err| {
                    inner.dispatch_task.wake();
                    inner.io_stopped(Some(err));
                    inner.insert_flags(Flags::BUF_R_READY);
                });
        }

        match result {
            Poll::Ready(Ok(())) => {
                inner.io_stopped(None);
                Poll::Ready(())
            }
            Poll::Ready(Err(e)) => {
                inner.io_stopped(Some(e));
                Poll::Ready(())
            }
            Poll::Pending => {
                if inner.flags.get().contains(Flags::IO_STOPPING_FILTERS) {
                    shutdown_filters(&self.0);
                }
                Poll::Pending
            }
        }
    }

    /// Get read buffer (async)
    pub async fn with_buf_async<F, R>(&self, f: F) -> Poll<()>
    where
        F: FnOnce(BytesVec) -> R,
        R: Future<Output = (BytesVec, io::Result<usize>)>,
    {
        let inner = &self.0 .0;

        // we already pushed new data to read buffer,
        // we have to wait for dispatcher to read data from buffer
        if inner.flags.get().is_read_buf_ready() {
            task::yield_to().await;
        }

        let mut buf = if inner.flags.get().is_read_buf_ready() {
            // read buffer is still not read by dispatcher
            // we cannot touch it
            inner.pool.get().get_read_buf()
        } else {
            inner
                .buffer
                .get_read_source()
                .unwrap_or_else(|| inner.pool.get().get_read_buf())
        };

        // make sure we've got room
        let remaining = buf.remaining_mut();
        let (hw, lw) = self.0.memory_pool().read_params().unpack();
        if remaining < lw {
            buf.reserve(hw - remaining);
        }
        let total = buf.len();

        // call provided callback
        let (buf, result) = f(buf).await;
        let total2 = buf.len();
        let nbytes = if total2 > total { total2 - total } else { 0 };
        let total = total2;

        if let Some(mut first_buf) = inner.buffer.get_read_source() {
            first_buf.extend_from_slice(&buf);
            inner.buffer.set_read_source(&self.0, first_buf);
        } else {
            inner.buffer.set_read_source(&self.0, buf);
        }

        // handle buffer changes
        if nbytes > 0 {
            let filter = self.0.filter();
            let res = match filter.process_read_buf(&self.0, &inner.buffer, 0, nbytes) {
                Ok(status) => {
                    if status.nbytes > 0 {
                        // check read back-pressure
                        if hw < inner.buffer.read_destination_size() {
                            log::trace!(
                                "{}: Io read buffer is too large {}, enable read back-pressure",
                                self.0.tag(),
                                total
                            );
                            inner.insert_flags(Flags::BUF_R_READY | Flags::BUF_R_FULL);
                        } else {
                            inner.insert_flags(Flags::BUF_R_READY);
                        }
                        log::trace!(
                            "{}: New {} bytes available, wakeup dispatcher",
                            self.0.tag(),
                            nbytes
                        );
                        // dest buffer has new data, wake up dispatcher
                        inner.dispatch_task.wake();
                    } else if inner.flags.get().contains(Flags::RD_NOTIFY) {
                        // in case of "notify" we must wake up dispatch task
                        // if we read any data from source
                        inner.dispatch_task.wake();
                    }

                    // while reading, filter wrote some data
                    // in that case filters need to process write buffers
                    // and potentialy wake write task
                    if status.need_write {
                        filter.process_write_buf(&self.0, &inner.buffer, 0)
                    } else {
                        Ok(())
                    }
                }
                Err(err) => Err(err),
            };

            if let Err(err) = res {
                inner.dispatch_task.wake();
                inner.io_stopped(Some(err));
                inner.insert_flags(Flags::BUF_R_READY);
            }
        }

        match result {
            Ok(n) => {
                if n == 0 {
                    inner.io_stopped(None);
                    Poll::Ready(())
                } else {
                    if inner.flags.get().contains(Flags::IO_STOPPING_FILTERS) {
                        shutdown_filters(&self.0);
                    }
                    Poll::Pending
                }
            }
            Err(e) => {
                inner.io_stopped(Some(e));
                Poll::Ready(())
            }
        }
    }
}

#[derive(Debug)]
/// Context for io write task
pub struct WriteContext(IoRef);

impl WriteContext {
    pub(crate) fn new(io: &IoRef) -> Self {
        Self(io.clone())
    }

    #[inline]
    /// Io tag
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[inline]
    /// Return memory pool for this context
    pub fn memory_pool(&self) -> PoolRef {
        self.0.memory_pool()
    }

    #[inline]
    /// Check readiness for write operations
    pub async fn ready(&self) -> WriteStatus {
        poll_fn(|cx| self.0.filter().poll_write_ready(cx)).await
    }

    #[inline]
    /// Check readiness for write operations
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        self.0.filter().poll_write_ready(cx)
    }

    /// Get write buffer
    pub fn with_buf<F>(&self, f: F) -> Poll<io::Result<()>>
    where
        F: FnOnce(&mut Option<BytesVec>) -> Poll<io::Result<()>>,
    {
        let inner = &self.0 .0;

        // call provided callback
        let (result, len) = inner.buffer.with_write_destination(&self.0, |buf| {
            let result = f(buf);
            (result, buf.as_ref().map(|b| b.len()).unwrap_or(0))
        });

        // if write buffer is smaller than high watermark value, turn off back-pressure
        let mut flags = inner.flags.get();
        if len == 0 {
            if flags.is_waiting_for_write() {
                flags.waiting_for_write_is_done();
                inner.dispatch_task.wake();
            }
        } else if flags.contains(Flags::WR_BACKPRESSURE)
            && len < inner.pool.get().write_params_high() << 1
        {
            flags.remove(Flags::WR_BACKPRESSURE);
            inner.dispatch_task.wake();
        }

        match result {
            Poll::Pending => flags.remove(Flags::WR_PAUSED),
            Poll::Ready(Ok(())) => flags.insert(Flags::WR_PAUSED),
            Poll::Ready(Err(_)) => {}
        }

        inner.flags.set(flags);
        result
    }

    /// Get write buffer (async)
    pub async fn with_buf_async<F, R>(&self, f: F) -> io::Result<()>
    where
        F: FnOnce(BytesVec) -> R,
        R: Future<Output = io::Result<()>>,
    {
        let inner = &self.0 .0;

        // running
        let mut flags = inner.flags.get();
        if flags.contains(Flags::WR_PAUSED) {
            flags.remove(Flags::WR_PAUSED);
            inner.flags.set(flags);
        }

        // buffer
        let buf = inner.buffer.get_write_destination();

        // call provided callback
        let result = if let Some(buf) = buf {
            if !buf.is_empty() {
                f(buf).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };

        // if write buffer is smaller than high watermark value, turn off back-pressure
        let mut flags = inner.flags.get();
        let len = inner.buffer.write_destination_size();

        if len == 0 {
            if flags.is_waiting_for_write() {
                flags.waiting_for_write_is_done();
                inner.dispatch_task.wake();
            }
            flags.insert(Flags::WR_PAUSED);
            inner.flags.set(flags);
        } else if flags.contains(Flags::WR_BACKPRESSURE)
            && len < inner.pool.get().write_params_high() << 1
        {
            flags.remove(Flags::WR_BACKPRESSURE);
            inner.flags.set(flags);
            inner.dispatch_task.wake();
        }

        result
    }

    #[inline]
    /// Indicate that write io task is stopped
    pub fn close(&self, err: Option<io::Error>) {
        self.0 .0.io_stopped(err);
    }
}

fn shutdown_filters(io: &IoRef) {
    let st = &io.0;
    let flags = st.flags.get();

    if !flags.intersects(Flags::IO_STOPPED | Flags::IO_STOPPING) {
        let filter = io.filter();
        match filter.shutdown(io, &st.buffer, 0) {
            Ok(Poll::Ready(())) => {
                st.dispatch_task.wake();
                st.insert_flags(Flags::IO_STOPPING);
            }
            Ok(Poll::Pending) => {
                // check read buffer, if buffer is not consumed it is unlikely
                // that filter will properly complete shutdown
                if flags.contains(Flags::RD_PAUSED)
                    || flags.contains(Flags::BUF_R_FULL | Flags::BUF_R_READY)
                {
                    st.dispatch_task.wake();
                    st.insert_flags(Flags::IO_STOPPING);
                }
            }
            Err(err) => {
                st.io_stopped(Some(err));
            }
        }
        if let Err(err) = filter.process_write_buf(io, &st.buffer, 0) {
            st.io_stopped(Some(err));
        }
    }
}
