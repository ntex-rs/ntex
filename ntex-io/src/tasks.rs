use std::{io, task::Context, task::Poll};

use ntex_bytes::{BytesMut, PoolRef};

use super::{io::Flags, IoRef, ReadStatus, WriteStatus};

pub struct ReadContext(IoRef);

impl ReadContext {
    pub(crate) fn new(io: &IoRef) -> Self {
        Self(io.clone())
    }

    #[inline]
    pub fn memory_pool(&self) -> PoolRef {
        self.0.memory_pool()
    }

    #[inline]
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        self.0.filter().poll_read_ready(cx)
    }

    #[inline]
    pub fn get_read_buf(&self) -> BytesMut {
        self.0
            .filter()
            .get_read_buf()
            .unwrap_or_else(|| self.0.memory_pool().get_read_buf())
    }

    #[inline]
    pub fn release_read_buf(&self, buf: BytesMut, nbytes: usize) {
        if buf.is_empty() {
            self.0.memory_pool().release_read_buf(buf);
        } else {
            let filter = self.0.filter();
            let mut dst = self.0 .0.read_buf.take();
            let result = filter.release_read_buf(&self.0, buf, &mut dst, nbytes);
            let nbytes = result.as_ref().map(|i| *i).unwrap_or(0);

            if let Some(dst) = dst {
                if nbytes > 0 {
                    if dst.len() > self.0.memory_pool().read_params().high as usize {
                        log::trace!(
                            "buffer is too large {}, enable read back-pressure",
                            dst.len()
                        );
                        self.0 .0.insert_flags(Flags::RD_READY | Flags::RD_BUF_FULL);
                    } else {
                        self.0 .0.insert_flags(Flags::RD_READY);
                        log::trace!("new {} bytes available, wakeup dispatcher", nbytes);
                    }
                    self.0 .0.dispatch_task.wake();
                }
                self.0 .0.read_buf.set(Some(dst));
            } else if nbytes > 0 {
                self.0 .0.dispatch_task.wake();
                self.0 .0.insert_flags(Flags::RD_READY);
            }

            if let Err(err) = result {
                self.0 .0.dispatch_task.wake();
                self.0 .0.insert_flags(Flags::RD_READY);
                self.0.want_shutdown(Some(err));
            }
        }

        if self.0.flags().contains(Flags::IO_STOPPING_FILTERS) {
            self.0 .0.shutdown_filters();
        }
    }

    #[inline]
    pub fn close(&self, err: Option<io::Error>) {
        self.0 .0.io_stopped(err);
    }
}

pub struct WriteContext(IoRef);

impl WriteContext {
    pub(crate) fn new(io: &IoRef) -> Self {
        Self(io.clone())
    }

    #[inline]
    pub fn memory_pool(&self) -> PoolRef {
        self.0.memory_pool()
    }

    #[inline]
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        self.0.filter().poll_write_ready(cx)
    }

    #[inline]
    pub fn get_write_buf(&self) -> Option<BytesMut> {
        self.0 .0.write_buf.take()
    }

    #[inline]
    pub fn release_write_buf(&self, buf: BytesMut) -> Result<(), io::Error> {
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
    pub fn close(&self, err: Option<io::Error>) {
        self.0 .0.io_stopped(err);
    }
}
