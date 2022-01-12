use std::{any, io, task::Context, task::Poll};

use ntex_bytes::BytesMut;

use super::io::Flags;
use super::{Filter, IoRef, ReadStatus, WriteStatus};

/// Default `Io` filter
pub struct Base(IoRef);

impl Base {
    pub(crate) fn new(inner: IoRef) -> Self {
        Base(inner)
    }
}

impl Filter for Base {
    #[inline]
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if let Some(hnd) = self.0 .0.handle.take() {
            let res = hnd.query(id);
            self.0 .0.handle.set(Some(hnd));
            res
        } else {
            None
        }
    }

    #[inline]
    fn poll_shutdown(&self) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        let flags = self.0.flags();

        if flags.intersects(Flags::IO_STOPPING) {
            Poll::Ready(ReadStatus::Terminate)
        } else if flags.intersects(Flags::RD_PAUSED | Flags::RD_BUF_FULL) {
            self.0 .0.read_task.register(cx.waker());
            Poll::Pending
        } else {
            self.0 .0.read_task.register(cx.waker());
            Poll::Ready(ReadStatus::Ready)
        }
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        let mut flags = self.0.flags();

        if flags.contains(Flags::IO_STOPPED) {
            Poll::Ready(WriteStatus::Terminate)
        } else if flags.intersects(Flags::IO_STOPPING) {
            Poll::Ready(WriteStatus::Shutdown(self.0 .0.disconnect_timeout.get()))
        } else if flags.contains(Flags::IO_STOPPING_FILTERS)
            && !flags.contains(Flags::IO_FILTERS_TIMEOUT)
        {
            flags.insert(Flags::IO_FILTERS_TIMEOUT);
            self.0.set_flags(flags);
            self.0 .0.write_task.register(cx.waker());
            Poll::Ready(WriteStatus::Timeout(self.0 .0.disconnect_timeout.get()))
        } else {
            self.0 .0.write_task.register(cx.waker());
            Poll::Ready(WriteStatus::Ready)
        }
    }

    #[inline]
    fn get_read_buf(&self) -> Option<BytesMut> {
        self.0 .0.read_buf.take()
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesMut> {
        self.0 .0.write_buf.take()
    }

    #[inline]
    fn release_read_buf(&self, buf: BytesMut) {
        self.0 .0.read_buf.set(Some(buf));
    }

    #[inline]
    fn process_read_buf(&self, _: &IoRef, n: usize) -> io::Result<(usize, usize)> {
        let buf = self.0 .0.read_buf.as_ptr();
        let ref_buf = unsafe { buf.as_ref().unwrap() };
        let total = ref_buf.as_ref().map(|v| v.len()).unwrap_or(0);
        Ok((total, n))
    }

    #[inline]
    fn release_write_buf(&self, buf: BytesMut) -> Result<(), io::Error> {
        let pool = self.0.memory_pool();
        if buf.is_empty() {
            pool.release_write_buf(buf);
        } else {
            if buf.len() >= pool.write_params_high() {
                self.0 .0.insert_flags(Flags::WR_BACKPRESSURE);
            }
            self.0 .0.write_buf.set(Some(buf));
            self.0 .0.write_task.wake();
        }
        Ok(())
    }
}

pub(crate) struct NullFilter;

const NULL: NullFilter = NullFilter;

impl NullFilter {
    pub(super) fn get() -> &'static dyn Filter {
        &NULL
    }
}

impl Filter for NullFilter {
    fn query(&self, _: any::TypeId) -> Option<Box<dyn any::Any>> {
        None
    }

    fn poll_shutdown(&self) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_read_ready(&self, _: &mut Context<'_>) -> Poll<ReadStatus> {
        Poll::Ready(ReadStatus::Terminate)
    }

    fn poll_write_ready(&self, _: &mut Context<'_>) -> Poll<WriteStatus> {
        Poll::Ready(WriteStatus::Terminate)
    }

    fn get_read_buf(&self) -> Option<BytesMut> {
        None
    }

    fn get_write_buf(&self) -> Option<BytesMut> {
        None
    }

    fn release_read_buf(&self, _: BytesMut) {}

    fn process_read_buf(&self, _: &IoRef, _: usize) -> io::Result<(usize, usize)> {
        Ok((0, 0))
    }

    fn release_write_buf(&self, _: BytesMut) -> Result<(), io::Error> {
        Ok(())
    }
}
