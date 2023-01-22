use std::{any, io, task::Context, task::Poll};

use super::buf::Stack;
use super::{io::Flags, Filter, IoRef, ReadStatus, WriteStatus};

/// Default `Io` filter
pub struct Base(IoRef);

impl Base {
    pub(crate) fn new(inner: IoRef) -> Self {
        Base(inner)
    }
}

pub struct Layer<F, L = Base>(pub(crate) F, L);

impl<F: Filter, L: FilterLayer> Layer<F, L> {
    pub(crate) fn new(f: F, l: L) -> Self {
        Self(f, l)
    }
}

pub(crate) struct NullFilter;

const NULL: NullFilter = NullFilter;

impl NullFilter {
    pub(super) fn get() -> &'static dyn FilterLayer {
        &NULL
    }
}

pub trait FilterLayer: 'static {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>>;

    fn process_read_buf(
        &self,
        io: &IoRef,
        stack: &mut Stack,
        idx: usize,
        nbytes: usize,
    ) -> io::Result<usize>;

    /// Process write buffer
    fn process_write_buf(
        &self,
        io: &IoRef,
        stack: &mut Stack,
        idx: usize,
    ) -> io::Result<()>;

    /// Gracefully shutdown filter
    fn shutdown(&self, io: &IoRef, stack: &mut Stack, idx: usize) -> io::Result<Poll<()>>;

    /// Check readiness for read operations
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus>;

    /// Check readiness for write operations
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus>;
}

impl FilterLayer for Base {
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
    fn process_read_buf(
        &self,
        _: &IoRef,
        _: &mut Stack,
        _: usize,
        nbytes: usize,
    ) -> io::Result<usize> {
        Ok(nbytes)
    }

    #[inline]
    fn process_write_buf(&self, _: &IoRef, _: &mut Stack, _: usize) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn shutdown(&self, _: &IoRef, _: &mut Stack, _: usize) -> io::Result<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}

impl<F, L> FilterLayer for Layer<F, L>
where
    F: Filter,
    L: FilterLayer,
{
    #[inline]
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        self.0.query(id).or_else(|| self.1.query(id))
    }

    #[inline]
    fn shutdown(&self, io: &IoRef, stack: &mut Stack, idx: usize) -> io::Result<Poll<()>> {
        let result1 = self.0.shutdown(&mut stack.buffer(io, idx))?;
        let result2 = self.1.shutdown(io, stack, idx + 1)?;

        if result1.is_pending() || result2.is_pending() {
            Ok(Poll::Pending)
        } else {
            Ok(Poll::Ready(()))
        }
    }

    #[inline]
    fn process_read_buf(
        &self,
        io: &IoRef,
        stack: &mut Stack,
        idx: usize,
        nbytes: usize,
    ) -> io::Result<usize> {
        let nbytes = self.1.process_read_buf(io, stack, idx + 1, nbytes)?;
        self.0.process_read_buf(&mut stack.buffer(io, idx), nbytes)
    }

    #[inline]
    fn process_write_buf(
        &self,
        io: &IoRef,
        stack: &mut Stack,
        idx: usize,
    ) -> io::Result<()> {
        self.0.process_write_buf(&mut stack.buffer(io, idx))?;
        self.1.process_write_buf(io, stack, idx + 1)
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        let res1 = self.0.poll_read_ready(cx);
        let res2 = self.1.poll_read_ready(cx);

        match res1 {
            Poll::Pending => Poll::Pending,
            Poll::Ready(ReadStatus::Ready) => res2,
            Poll::Ready(ReadStatus::Terminate) => Poll::Ready(ReadStatus::Terminate),
        }
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        let res1 = self.0.poll_write_ready(cx);
        let res2 = self.1.poll_write_ready(cx);

        match res1 {
            Poll::Pending => Poll::Pending,
            Poll::Ready(WriteStatus::Ready) => res2,
            Poll::Ready(WriteStatus::Terminate) => Poll::Ready(WriteStatus::Terminate),
            Poll::Ready(WriteStatus::Shutdown(t)) => {
                if res2 == Poll::Ready(WriteStatus::Terminate) {
                    Poll::Ready(WriteStatus::Terminate)
                } else {
                    Poll::Ready(WriteStatus::Shutdown(t))
                }
            }
            Poll::Ready(WriteStatus::Timeout(t)) => match res2 {
                Poll::Ready(WriteStatus::Terminate) => Poll::Ready(WriteStatus::Terminate),
                Poll::Ready(WriteStatus::Shutdown(t)) => {
                    Poll::Ready(WriteStatus::Shutdown(t))
                }
                _ => Poll::Ready(WriteStatus::Timeout(t)),
            },
        }
    }
}

impl FilterLayer for NullFilter {
    #[inline]
    fn query(&self, _: any::TypeId) -> Option<Box<dyn any::Any>> {
        None
    }

    #[inline]
    fn poll_read_ready(&self, _: &mut Context<'_>) -> Poll<ReadStatus> {
        Poll::Ready(ReadStatus::Terminate)
    }

    #[inline]
    fn poll_write_ready(&self, _: &mut Context<'_>) -> Poll<WriteStatus> {
        Poll::Ready(WriteStatus::Terminate)
    }

    #[inline]
    fn process_read_buf(
        &self,
        _: &IoRef,
        _: &mut Stack,
        _: usize,
        _: usize,
    ) -> io::Result<usize> {
        Ok(0)
    }

    #[inline]
    fn process_write_buf(&self, _: &IoRef, _: &mut Stack, _: usize) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn shutdown(&self, _: &IoRef, _: &mut Stack, _: usize) -> io::Result<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}
