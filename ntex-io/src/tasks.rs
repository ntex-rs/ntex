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
    /// Return memory pool for this context
    pub fn memory_pool(&self) -> PoolRef {
        self.0.memory_pool()
    }

    #[inline]
    /// Check readiness for read operations
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        self.0.filter().poll_read_ready(cx)
    }

    #[inline]
    /// Get read buffer
    pub fn get_read_buf(&self) -> BytesVec {
        self.0
             .0
            .read_buf
            .take()
            .unwrap_or_else(|| self.0.memory_pool().get_read_buf())
    }

    #[inline]
    /// Release read buffer after io read operations
    pub fn release_read_buf(&self, buf: BytesVec, nbytes: usize) {
        if buf.is_empty() {
            self.0.memory_pool().release_read_buf(buf);
        } else {
            self.0 .0.read_buf.set(Some(buf));
            let filter = self.0.filter();
            match filter.process_read_buf(&self.0, nbytes) {
                Ok((total, nbytes)) => {
                    if nbytes > 0 {
                        if total > self.0.memory_pool().read_params().high as usize {
                            log::trace!(
                                "buffer is too large {}, enable read back-pressure",
                                total
                            );
                            self.0 .0.insert_flags(Flags::RD_READY | Flags::RD_BUF_FULL);
                        }
                        self.0 .0.dispatch_task.wake();
                        self.0 .0.insert_flags(Flags::RD_READY);
                        log::trace!("new {} bytes available, wakeup dispatcher", nbytes);
                    }
                }
                Err(err) => {
                    self.0 .0.dispatch_task.wake();
                    self.0 .0.insert_flags(Flags::RD_READY);
                    self.0.want_shutdown(Some(err));
                }
            }
        }

        if self.0.flags().contains(Flags::IO_STOPPING_FILTERS) {
            self.0 .0.shutdown_filters();
        }
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
        self.0 .0.write_buf.take()
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
            self.0 .0.write_buf.set(Some(buf))
        }

        if self.0.flags().contains(Flags::IO_STOPPING_FILTERS) {
            self.0 .0.shutdown_filters();
        }

        Ok(())
    }

    #[inline]
    /// Indicate that io task is stopped
    pub fn close(&self, err: Option<io::Error>) {
        self.0 .0.io_stopped(err);
    }
}
