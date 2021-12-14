use std::{io, task::Context, task::Poll};

use ntex_bytes::{BytesMut, PoolRef};

use super::{state::Flags, IoRef, WriteReadiness};

pub struct ReadState(pub(super) IoRef);

impl ReadState {
    #[inline]
    pub fn memory_pool(&self) -> PoolRef {
        self.0 .0.pool.get()
    }

    #[inline]
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        self.0 .0.filter.get().poll_read_ready(cx)
    }

    #[inline]
    pub fn close(&self, err: Option<io::Error>) {
        self.0 .0.filter.get().read_closed(err);
    }

    #[inline]
    pub fn get_read_buf(&self) -> BytesMut {
        self.0
             .0
            .filter
            .get()
            .get_read_buf()
            .unwrap_or_else(|| self.0 .0.pool.get().get_read_buf())
    }

    #[inline]
    pub fn release_read_buf(
        &self,
        buf: BytesMut,
        new_bytes: usize,
    ) -> Result<(), io::Error> {
        if buf.is_empty() {
            self.0 .0.pool.get().release_read_buf(buf);
            Ok(())
        } else {
            let mut flags = self.0 .0.flags.get();

            // notify dispatcher
            if new_bytes > 0 {
                flags.insert(Flags::RD_READY);
                self.0 .0.flags.set(flags);
                self.0 .0.dispatch_task.wake();
            }
            self.0 .0.filter.get().release_read_buf(buf, new_bytes)?;

            if flags.contains(Flags::IO_FILTERS) {
                self.0 .0.shutdown_filters(&self.0)?;
            }
            Ok(())
        }
    }
}

pub struct WriteState(pub(super) IoRef);

impl WriteState {
    #[inline]
    pub fn memory_pool(&self) -> PoolRef {
        self.0 .0.pool.get()
    }

    #[inline]
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), WriteReadiness>> {
        self.0 .0.filter.get().poll_write_ready(cx)
    }

    #[inline]
    pub fn close(&self, err: Option<io::Error>) {
        self.0 .0.filter.get().write_closed(err)
    }

    #[inline]
    pub fn get_write_buf(&self) -> Option<BytesMut> {
        self.0 .0.write_buf.take()
    }

    #[inline]
    pub fn release_write_buf(&self, buf: BytesMut) -> Result<(), io::Error> {
        let pool = self.0 .0.pool.get();
        let mut flags = self.0 .0.flags.get();

        if buf.is_empty() {
            pool.release_write_buf(buf);
            if flags.intersects(Flags::WR_WAIT | Flags::WR_BACKPRESSURE) {
                flags.remove(Flags::WR_WAIT | Flags::WR_BACKPRESSURE);
                self.0 .0.flags.set(flags);
                self.0 .0.dispatch_task.wake();
            }
        } else {
            // if write buffer is smaller than high watermark value, turn off back-pressure
            if buf.len() < pool.write_params_high() << 1
                && flags.contains(Flags::WR_BACKPRESSURE)
            {
                flags.remove(Flags::WR_BACKPRESSURE);
                self.0 .0.flags.set(flags);
                self.0 .0.dispatch_task.wake();
            }
            self.0 .0.write_buf.set(Some(buf))
        }

        if flags.contains(Flags::IO_FILTERS) {
            self.0 .0.shutdown_filters(&self.0)?;
        }
        Ok(())
    }
}
