use std::{any, io, task::Context, task::Poll};

use crate::{FilterCtx, FilterLayer, Flags, IoRef, Readiness};

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
        ctx: FilterCtx<'_>,
        nbytes: usize,
    ) -> io::Result<FilterReadStatus>;

    /// Process write buffer
    fn process_write_buf(&self, ctx: FilterCtx<'_>) -> io::Result<()>;

    /// Gracefully shutdown filter
    fn shutdown(&self, ctx: FilterCtx<'_>) -> io::Result<Poll<()>>;

    /// Check readiness for read operations
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness>;

    /// Check readiness for write operations
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness>;
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
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        let flags = self.0.flags();

        if flags.intersects(Flags::IO_STOPPING | Flags::IO_STOPPED) {
            Poll::Ready(Readiness::Terminate)
        } else {
            self.0 .0.read_task.register(cx.waker());

            if flags.intersects(Flags::IO_STOPPING_FILTERS) {
                Poll::Ready(Readiness::Ready)
            } else if flags.cannot_read() {
                Poll::Pending
            } else {
                Poll::Ready(Readiness::Ready)
            }
        }
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        let flags = self.0.flags();

        if flags.is_stopped() {
            Poll::Ready(Readiness::Terminate)
        } else {
            self.0 .0.write_task.register(cx.waker());

            if flags.contains(Flags::IO_STOPPING) {
                Poll::Ready(Readiness::Shutdown)
            } else if flags.contains(Flags::WR_PAUSED) {
                Poll::Pending
            } else {
                Poll::Ready(Readiness::Ready)
            }
        }
    }

    #[inline]
    fn process_read_buf(
        &self,
        _: FilterCtx<'_>,
        nbytes: usize,
    ) -> io::Result<FilterReadStatus> {
        Ok(FilterReadStatus {
            nbytes,
            need_write: false,
        })
    }

    #[inline]
    fn process_write_buf(&self, ctx: FilterCtx<'_>) -> io::Result<()> {
        ctx.stack.with_write_destination(ctx.io, |buf| {
            if let Some(buf) = buf {
                let len = buf.len();
                let flags = self.0.flags();
                if len > 0 && flags.contains(Flags::WR_PAUSED) {
                    self.0 .0.remove_flags(Flags::WR_PAUSED);
                    self.0 .0.write_task.wake();
                }
                if len >= self.0.memory_pool().write_params_high()
                    && !flags.contains(Flags::BUF_W_BACKPRESSURE)
                {
                    self.0 .0.insert_flags(Flags::BUF_W_BACKPRESSURE);
                    self.0 .0.dispatch_task.wake();
                }
            }
        });
        Ok(())
    }

    #[inline]
    fn shutdown(&self, _: FilterCtx<'_>) -> io::Result<Poll<()>> {
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
    fn shutdown(&self, ctx: FilterCtx<'_>) -> io::Result<Poll<()>> {
        let result1 = ctx.write_buf(|buf| self.0.shutdown(buf))?;
        self.process_write_buf(ctx)?;
        let result2 = self.1.shutdown(ctx.next())?;

        if result1.is_pending() || result2.is_pending() {
            Ok(Poll::Pending)
        } else {
            Ok(Poll::Ready(()))
        }
    }

    #[inline]
    fn process_read_buf(
        &self,
        ctx: FilterCtx<'_>,
        nbytes: usize,
    ) -> io::Result<FilterReadStatus> {
        let status = self.1.process_read_buf(ctx.next(), nbytes)?;
        ctx.read_buf(status.nbytes, |buf| {
            self.0.process_read_buf(buf).map(|nbytes| FilterReadStatus {
                nbytes,
                need_write: status.need_write || buf.need_write.get(),
            })
        })
    }

    #[inline]
    fn process_write_buf(&self, ctx: FilterCtx<'_>) -> io::Result<()> {
        ctx.write_buf(|buf| self.0.process_write_buf(buf))?;
        self.1.process_write_buf(ctx.next())
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        Readiness::merge(self.0.poll_read_ready(cx), self.1.poll_read_ready(cx))
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        Readiness::merge(self.0.poll_write_ready(cx), self.1.poll_write_ready(cx))
    }
}

impl Filter for NullFilter {
    #[inline]
    fn query(&self, _: any::TypeId) -> Option<Box<dyn any::Any>> {
        None
    }

    #[inline]
    fn poll_read_ready(&self, _: &mut Context<'_>) -> Poll<Readiness> {
        Poll::Ready(Readiness::Terminate)
    }

    #[inline]
    fn poll_write_ready(&self, _: &mut Context<'_>) -> Poll<Readiness> {
        Poll::Ready(Readiness::Terminate)
    }

    #[inline]
    fn process_read_buf(&self, _: FilterCtx<'_>, _: usize) -> io::Result<FilterReadStatus> {
        Ok(Default::default())
    }

    #[inline]
    fn process_write_buf(&self, _: FilterCtx<'_>) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn shutdown(&self, _: FilterCtx<'_>) -> io::Result<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}
