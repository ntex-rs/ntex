use std::{io, task::Context, task::Poll};

use ntex_bytes::{BytesVec, PoolRef};

use super::{io::Flags, IoRef, ReadStatus, WriteStatus};

/// Context for io read task
pub struct ReadContext(IoRef);

impl ReadContext {
    pub(crate) fn new(io: &IoRef) -> Self {
        Self(io.clone())
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
        let mut stack = self.0 .0.buffer.borrow_mut();
        let is_write_sleep = stack.last_write_buf_size() == 0;
        let mut buf = stack
            .last_read_buf()
            .take()
            .unwrap_or_else(|| self.0.memory_pool().get_read_buf());

        let total = buf.len();
        let (hw, lw) = self.0.memory_pool().read_params().unpack();

        // call provided callback
        let result = f(&mut buf, hw, lw);

        // handle buffer changes
        if buf.is_empty() {
            self.0.memory_pool().release_read_buf(buf);
        } else {
            let total2 = buf.len();
            let nbytes = if total2 > total { total2 - total } else { 0 };
            *stack.last_read_buf() = Some(buf);

            if nbytes > 0 {
                let buf_full = nbytes >= hw;
                match self
                    .0
                    .filter()
                    .process_read_buf(&self.0, &mut stack, 0, nbytes)
                {
                    Ok(status) => {
                        if status.nbytes > 0 {
                            if buf_full || stack.first_read_buf_size() >= hw {
                                log::trace!(
                                    "io read buffer is too large {}, enable read back-pressure",
                                    total2
                                );
                                self.0
                                     .0
                                    .insert_flags(Flags::RD_READY | Flags::RD_BUF_FULL);
                            } else {
                                self.0 .0.insert_flags(Flags::RD_READY);
                            }
                            self.0 .0.dispatch_task.wake();
                            log::trace!(
                                "new {} bytes available, wakeup dispatcher",
                                nbytes,
                            );
                        } else if buf_full {
                            // read task is paused because of read back-pressure
                            // but there is no new data in top most read buffer
                            // so we need to wake up read task to read more data
                            // otherwise read task would sleep forever
                            self.0 .0.read_task.wake();
                        }

                        // while reading, filter wrote some data
                        // in that case filters need to process write buffers
                        // and potentialy wake write task
                        if status.need_write {
                            if let Err(err) =
                                self.0.filter().process_write_buf(&self.0, &mut stack, 0)
                            {
                                self.0 .0.dispatch_task.wake();
                                self.0 .0.insert_flags(Flags::RD_READY);
                                self.0 .0.init_shutdown(Some(err), &self.0);
                            }
                            if is_write_sleep && stack.last_write_buf_size() != 0 {
                                self.0 .0.write_task.wake();
                            }
                        }
                    }
                    Err(err) => {
                        self.0 .0.dispatch_task.wake();
                        self.0 .0.insert_flags(Flags::RD_READY);
                        self.0 .0.init_shutdown(Some(err), &self.0);
                    }
                }
            }
        }

        let result = match result {
            Poll::Ready(Ok(())) => {
                self.0 .0.io_stopped(None);
                Poll::Ready(())
            }
            Poll::Ready(Err(e)) => {
                self.0 .0.io_stopped(Some(e));
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        };

        drop(stack);
        if self.0.flags().contains(Flags::IO_STOPPING_FILTERS) {
            self.0 .0.shutdown_filters(&self.0);
        }
        result
    }

    #[inline]
    /// Indicate that io task is stopped
    pub fn close(&self, err: Option<io::Error>) {
        self.0 .0.io_stopped(err);
    }
}

/// Context for io write task
pub struct WriteContext(IoRef);

impl WriteContext {
    pub(crate) fn new(io: &IoRef) -> Self {
        Self(io.clone())
    }

    #[inline]
    /// Return memory pool for this context
    pub fn memory_pool(&self) -> PoolRef {
        self.0.memory_pool()
    }

    #[inline]
    /// Check readiness for write operations
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        self.0.filter().poll_write_ready(cx)
    }

    #[inline]
    /// Get write buffer
    pub fn get_write_buf(&self) -> Option<BytesVec> {
        self.0 .0.buffer.borrow_mut().last_write_buf().take()
    }

    #[inline]
    /// Release write buffer after io write operations
    pub fn release_write_buf(&self, buf: BytesVec) -> Result<(), io::Error> {
        let pool = self.0.memory_pool();
        let mut flags = self.0.flags();

        if buf.is_empty() {
            pool.release_write_buf(buf);
            if flags.intersects(Flags::WR_WAIT | Flags::WR_BACKPRESSURE) {
                flags.remove(Flags::WR_WAIT | Flags::WR_BACKPRESSURE);
                self.0.set_flags(flags);
                self.0 .0.dispatch_task.wake();
            }
        } else {
            // if write buffer is smaller than high watermark value, turn off back-pressure
            if flags.contains(Flags::WR_BACKPRESSURE)
                && buf.len() < pool.write_params_high() << 1
            {
                flags.remove(Flags::WR_BACKPRESSURE);
                self.0.set_flags(flags);
                self.0 .0.dispatch_task.wake();
            }
            self.0 .0.buffer.borrow_mut().set_last_write_buf(buf);
        }

        if self.0.flags().contains(Flags::IO_STOPPING_FILTERS) {
            self.0 .0.shutdown_filters(&self.0);
        }

        Ok(())
    }

    #[inline]
    /// Indicate that io task is stopped
    pub fn close(&self, err: Option<io::Error>) {
        self.0 .0.io_stopped(err);
    }
}
