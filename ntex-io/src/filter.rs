use std::{any, cell::Cell, io, task::Context, task::Poll};

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
pub struct Layer<F, L = Base>(pub(crate) F, L, Cell<bool>);

impl<F: FilterLayer, L: Filter> Layer<F, L> {
    pub(crate) fn new(f: F, l: L) -> Self {
        Self(f, l, Cell::new(false))
    }
}

pub(crate) struct NullFilter;

const NULL: NullFilter = NullFilter;

impl NullFilter {
    pub(super) const fn get() -> &'static dyn Filter {
        &NULL
    }
}

pub trait Filter: 'static {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>>;

    fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()>;

    /// Process write buffer
    fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()>;

    /// Gracefully shutdown filter
    fn shutdown(&self, ctx: &mut FilterCtx<'_>) -> io::Result<Poll<()>>;

    /// Check readiness for read operations
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness>;

    /// Check readiness for write operations
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness>;
}

impl Filter for Base {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if let Some(hnd) = self.0.0.handle.take() {
            let res = hnd.query(id);
            self.0.0.handle.set(Some(hnd));
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
            self.0.0.read_task.register(cx.waker());

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
            self.0.0.write_task.register(cx.waker());

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
    fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
        if let Some(buf) = ctx.read_buffer.take() {
            ctx.set_base_read_buf(buf);
        }
        Ok(())
    }

    #[inline]
    fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
        let buf = ctx.write_destination();
        let len = buf.len();
        let flags = self.0.flags();
        if len > 0 && flags.contains(Flags::WR_PAUSED) {
            self.0.0.remove_flags(Flags::WR_PAUSED);
            self.0.0.write_task.wake();
        }
        if len >= self.0.cfg().write_buf().high
            && !flags.contains(Flags::BUF_W_BACKPRESSURE)
        {
            self.0.0.insert_flags(Flags::BUF_W_BACKPRESSURE);
            self.0.0.dispatch_task.wake();
        }
        Ok(())
    }

    #[inline]
    fn shutdown(&self, _: &mut FilterCtx<'_>) -> io::Result<Poll<()>> {
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
    fn shutdown(&self, ctx: &mut FilterCtx<'_>) -> io::Result<Poll<()>> {
        if !self.2.get() {
            if ctx.with_buffer(|buf| self.0.shutdown(buf))?.is_ready() {
                self.2.set(true);
                self.process_write_buf(ctx)?;
            } else {
                return Ok(Poll::Pending);
            }
        }
        ctx.with_next(|ctx| self.1.shutdown(ctx))
    }

    #[inline]
    fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
        ctx.with_next(|ctx| self.1.process_read_buf(ctx))?;
        if self.2.get() {
            Ok(())
        } else {
            ctx.with_buffer(|buf| self.0.process_read_buf(buf))
        }
    }

    #[inline]
    fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
        if !self.2.get() {
            ctx.with_buffer(|buf| self.0.process_write_buf(buf))?;
        }
        ctx.with_next(|ctx| self.1.process_write_buf(ctx))
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
    fn process_read_buf(&self, _: &mut FilterCtx<'_>) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn process_write_buf(&self, _: &mut FilterCtx<'_>) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn shutdown(&self, _: &mut FilterCtx<'_>) -> io::Result<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}
