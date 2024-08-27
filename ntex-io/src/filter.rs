use std::{any, io, task::Context, task::Poll};

use crate::{buf::Stack, FilterLayer, Flags, IoRef, ReadStatus, WriteStatus};

#[derive(Debug)]
/// Default `Io` filter
pub struct Base(IoRef);

impl Base {
    pub(crate) fn new(inner: IoRef) -> Self {
        Base(inner)
    }
}

#[derive(Debug)]
pub struct Layer<F, L = Base>(pub(crate) F, L);

impl<F: FilterLayer, L: Filter> Layer<F, L> {
    pub(crate) fn new(f: F, l: L) -> Self {
        Self(f, l)
    }
}

pub(crate) struct NullFilter;

const NULL: NullFilter = NullFilter;

impl NullFilter {
    pub(super) const fn get() -> &'static dyn Filter {
        &NULL
    }
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct FilterReadStatus {
    pub nbytes: usize,
    pub need_write: bool,
}

pub trait Filter: 'static {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>>;

    fn process_read_buf(
        &self,
        io: &IoRef,
        stack: &Stack,
        idx: usize,
        nbytes: usize,
    ) -> io::Result<FilterReadStatus>;

    /// Process write buffer
    fn process_write_buf(&self, io: &IoRef, stack: &Stack, idx: usize) -> io::Result<()>;

    /// Gracefully shutdown filter
    fn shutdown(&self, io: &IoRef, stack: &Stack, idx: usize) -> io::Result<Poll<()>>;

    /// Check readiness for read operations
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus>;

    /// Check readiness for write operations
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus>;
}

impl Filter for Base {
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

        if flags.intersects(Flags::IO_STOPPING | Flags::IO_STOPPED) {
            Poll::Ready(ReadStatus::Terminate)
        } else {
            self.0 .0.read_task.register(cx.waker());

            if flags.intersects(Flags::IO_STOPPING_FILTERS) {
                Poll::Ready(ReadStatus::Ready)
            } else if flags.cannot_read() {
                Poll::Pending
            } else {
                Poll::Ready(ReadStatus::Ready)
            }
        }
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        let mut flags = self.0.flags();

        if flags.contains(Flags::IO_STOPPED) {
            Poll::Ready(WriteStatus::Terminate)
        } else if flags.intersects(Flags::IO_STOPPING) {
            Poll::Ready(WriteStatus::Shutdown(
                self.0 .0.disconnect_timeout.get().into(),
            ))
        } else if flags.contains(Flags::IO_STOPPING_FILTERS)
            && !flags.contains(Flags::IO_FILTERS_TIMEOUT)
        {
            flags.insert(Flags::IO_FILTERS_TIMEOUT);
            self.0.set_flags(flags);
            self.0 .0.write_task.register(cx.waker());
            Poll::Ready(WriteStatus::Timeout(
                self.0 .0.disconnect_timeout.get().into(),
            ))
        } else if flags.intersects(Flags::WR_PAUSED) {
            self.0 .0.write_task.register(cx.waker());
            Poll::Pending
        } else {
            self.0 .0.write_task.register(cx.waker());
            Poll::Ready(WriteStatus::Ready)
        }
    }

    #[inline]
    fn process_read_buf(
        &self,
        _: &IoRef,
        _: &Stack,
        _: usize,
        nbytes: usize,
    ) -> io::Result<FilterReadStatus> {
        Ok(FilterReadStatus {
            nbytes,
            need_write: false,
        })
    }

    #[inline]
    fn process_write_buf(&self, io: &IoRef, s: &Stack, _: usize) -> io::Result<()> {
        s.with_write_destination(io, |buf| {
            if let Some(buf) = buf {
                let len = buf.len();
                let flags = self.0.flags();
                if len > 0 && flags.contains(Flags::WR_PAUSED) {
                    self.0 .0.remove_flags(Flags::WR_PAUSED);
                    self.0 .0.write_task.wake();
                }
                if len >= self.0.memory_pool().write_params_high()
                    && !flags.contains(Flags::WR_BACKPRESSURE)
                {
                    self.0 .0.insert_flags(Flags::WR_BACKPRESSURE);
                    self.0 .0.dispatch_task.wake();
                }
            }
        });
        Ok(())
    }

    #[inline]
    fn shutdown(&self, _: &IoRef, _: &Stack, _: usize) -> io::Result<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}

impl<F, L> Filter for Layer<F, L>
where
    F: FilterLayer,
    L: Filter,
{
    #[inline]
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        self.0.query(id).or_else(|| self.1.query(id))
    }

    #[inline]
    fn shutdown(&self, io: &IoRef, stack: &Stack, idx: usize) -> io::Result<Poll<()>> {
        let result1 = stack.write_buf(io, idx, |buf| self.0.shutdown(buf))?;
        self.process_write_buf(io, stack, idx)?;

        let result2 = if F::BUFFERS {
            self.1.shutdown(io, stack, idx + 1)?
        } else {
            self.1.shutdown(io, stack, idx)?
        };

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
        stack: &Stack,
        idx: usize,
        nbytes: usize,
    ) -> io::Result<FilterReadStatus> {
        let status = if F::BUFFERS {
            self.1.process_read_buf(io, stack, idx + 1, nbytes)?
        } else {
            self.1.process_read_buf(io, stack, idx, nbytes)?
        };
        stack.read_buf(io, idx, status.nbytes, |buf| {
            self.0.process_read_buf(buf).map(|nbytes| FilterReadStatus {
                nbytes,
                need_write: status.need_write || buf.need_write.get(),
            })
        })
    }

    #[inline]
    fn process_write_buf(&self, io: &IoRef, stack: &Stack, idx: usize) -> io::Result<()> {
        stack.write_buf(io, idx, |buf| self.0.process_write_buf(buf))?;

        if F::BUFFERS {
            self.1.process_write_buf(io, stack, idx + 1)
        } else {
            self.1.process_write_buf(io, stack, idx)
        }
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

impl Filter for NullFilter {
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
        _: &Stack,
        _: usize,
        _: usize,
    ) -> io::Result<FilterReadStatus> {
        Ok(Default::default())
    }

    #[inline]
    fn process_write_buf(&self, _: &IoRef, _: &Stack, _: usize) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn shutdown(&self, _: &IoRef, _: &Stack, _: usize) -> io::Result<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}
