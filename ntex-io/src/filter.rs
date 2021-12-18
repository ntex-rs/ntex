use std::{any, io, rc::Rc, task::Context, task::Poll};

use ntex_bytes::BytesMut;

use super::state::{Flags, IoRef, IoStateInner};
use super::{Filter, ReadFilter, WriteFilter, WriteReadiness};

pub struct DefaultFilter(Rc<IoStateInner>);

impl DefaultFilter {
    pub(crate) fn new(inner: Rc<IoStateInner>) -> Self {
        DefaultFilter(inner)
    }
}

impl Filter for DefaultFilter {
    #[inline]
    fn shutdown(&self, _: &IoRef) -> Poll<Result<(), io::Error>> {
        let mut flags = self.0.flags.get();
        if !flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            flags.insert(Flags::IO_SHUTDOWN);
            self.0.flags.set(flags);
            self.0.read_task.wake();
            self.0.write_task.wake();
        }

        Poll::Ready(Ok(()))
    }

    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if let Some(hnd) = self.0.handle.take() {
            let res = hnd.query(id);
            self.0.handle.set(Some(hnd));
            res
        } else {
            None
        }
    }
}

impl ReadFilter for DefaultFilter {
    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
        let flags = self.0.flags.get();

        if flags.intersects(Flags::IO_ERR | Flags::IO_SHUTDOWN) {
            Poll::Ready(Err(()))
        } else if flags.intersects(Flags::RD_PAUSED) {
            self.0.read_task.register(cx.waker());
            Poll::Pending
        } else {
            self.0.read_task.register(cx.waker());
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    fn read_closed(&self, err: Option<io::Error>) {
        self.0.set_error(err);
    }

    #[inline]
    fn get_read_buf(&self) -> Option<BytesMut> {
        self.0.read_buf.take()
    }

    #[inline]
    fn release_read_buf(
        &self,
        buf: BytesMut,
        new_bytes: usize,
    ) -> Result<bool, io::Error> {
        let mut flags = self.0.flags.get();

        if new_bytes > 0 {
            if buf.len() > self.0.pool.get().read_params().high as usize {
                log::trace!(
                    "buffer is too large {}, enable read back-pressure",
                    buf.len()
                );
                flags.insert(Flags::RD_READY | Flags::RD_BUF_FULL);
            } else {
                flags.insert(Flags::RD_READY);
            }
            self.0.flags.set(flags);
        }

        self.0.read_buf.set(Some(buf));
        Ok(false)
    }
}

impl WriteFilter for DefaultFilter {
    #[inline]
    fn poll_write_ready(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), WriteReadiness>> {
        let mut flags = self.0.flags.get();

        if flags.contains(Flags::IO_ERR) {
            Poll::Ready(Err(WriteReadiness::Terminate))
        } else if flags.intersects(Flags::IO_SHUTDOWN) {
            Poll::Ready(Err(WriteReadiness::Shutdown(
                self.0.disconnect_timeout.get(),
            )))
        } else if flags.contains(Flags::IO_FILTERS)
            && !flags.contains(Flags::IO_FILTERS_TO)
        {
            flags.insert(Flags::IO_FILTERS_TO);
            self.0.flags.set(flags);
            self.0.write_task.register(cx.waker());
            Poll::Ready(Err(WriteReadiness::Timeout(
                self.0.disconnect_timeout.get(),
            )))
        } else {
            self.0.write_task.register(cx.waker());
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    fn write_closed(&self, err: Option<io::Error>) {
        self.0.set_error(err);
        self.0.handle.take();
        self.0.insert_flags(Flags::IO_CLOSED);
        self.0.dispatch_task.wake();
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesMut> {
        self.0.write_buf.take()
    }

    #[inline]
    fn release_write_buf(&self, buf: BytesMut) -> Result<bool, io::Error> {
        let pool = self.0.pool.get();
        if buf.is_empty() {
            pool.release_write_buf(buf);
        } else {
            self.0.write_buf.set(Some(buf));
            self.0.write_task.wake();
        }
        Ok(false)
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
    fn shutdown(&self, _: &IoRef) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn query(&self, _: any::TypeId) -> Option<Box<dyn any::Any>> {
        None
    }
}

impl ReadFilter for NullFilter {
    fn poll_read_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
        Poll::Ready(Err(()))
    }

    fn read_closed(&self, _: Option<io::Error>) {}

    fn get_read_buf(&self) -> Option<BytesMut> {
        None
    }

    fn release_read_buf(&self, _: BytesMut, _: usize) -> Result<bool, io::Error> {
        Ok(true)
    }
}

impl WriteFilter for NullFilter {
    fn poll_write_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), WriteReadiness>> {
        Poll::Ready(Err(WriteReadiness::Terminate))
    }

    fn write_closed(&self, _: Option<io::Error>) {}

    fn get_write_buf(&self) -> Option<BytesMut> {
        None
    }

    fn release_write_buf(&self, _: BytesMut) -> Result<bool, io::Error> {
        Ok(true)
    }
}
