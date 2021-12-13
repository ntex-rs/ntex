use std::{io, rc::Rc, task::Context, task::Poll};

use ntex_bytes::{BytesMut, PoolRef};
use ntex_util::time::Seconds;

use super::{state::Flags, state::IoStateInner, WriteReadiness};

pub struct ReadState(pub(super) Rc<IoStateInner>);

impl ReadState {
    #[inline]
    pub fn memory_pool(&self) -> PoolRef {
        self.0.pool.get()
    }

    #[inline]
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        self.0.filter.get().poll_read_ready(cx)
    }

    #[inline]
    pub fn close(&self, err: Option<io::Error>) {
        self.0.filter.get().read_closed(err);
    }

    #[inline]
    pub fn get_read_buf(&self) -> BytesMut {
        self.0
            .filter
            .get()
            .get_read_buf()
            .unwrap_or_else(|| self.0.pool.get().get_read_buf())
    }

    #[inline]
    pub fn release_read_buf(&self, buf: BytesMut, new_bytes: usize) {
        if buf.is_empty() {
            self.0.pool.get().release_read_buf(buf);
        } else {
            self.0.filter.get().release_read_buf(buf, new_bytes);
        }
    }
}

pub struct WriteState(pub(super) Rc<IoStateInner>);

impl WriteState {
    #[inline]
    pub fn memory_pool(&self) -> PoolRef {
        self.0.pool.get()
    }

    #[inline]
    pub fn disconnect_timeout(&self) -> Seconds {
        self.0.disconnect_timeout.get()
    }

    #[inline]
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), WriteReadiness>> {
        self.0.filter.get().poll_write_ready(cx)
    }

    #[inline]
    pub fn close(&self, err: Option<io::Error>) {
        self.0.filter.get().write_closed(err)
    }

    #[inline]
    pub fn get_write_buf(&self) -> Option<BytesMut> {
        self.0.write_buf.take()
    }

    #[inline]
    pub fn release_write_buf(&self, buf: BytesMut) {
        let pool = self.0.pool.get();
        if buf.is_empty() {
            pool.release_write_buf(buf);

            let mut flags = self.0.flags.get();
            if flags.intersects(Flags::WR_WAIT | Flags::WR_BACKPRESSURE) {
                flags.remove(Flags::WR_WAIT | Flags::WR_BACKPRESSURE);
                self.0.flags.set(flags);
                self.0.dispatch_task.wake();
            }
        } else {
            // if write buffer is smaller than high watermark value, turn off back-pressure
            if buf.len() < pool.write_params_high() << 1 {
                let mut flags = self.0.flags.get();
                if flags.contains(Flags::WR_BACKPRESSURE) {
                    flags.remove(Flags::WR_BACKPRESSURE);
                    self.0.flags.set(flags);
                    self.0.dispatch_task.wake();
                }
            }
            self.0.write_buf.set(Some(buf))
        }
    }
}
