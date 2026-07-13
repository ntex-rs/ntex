use std::{any, cell::Cell, io, task::Context, task::Poll};

use crate::{FilterCtx, FilterLayer, IoRef, Readiness};

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
    /// Accesses internal filter information.
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>>;

    /// Processes incoming read-buffer data.
    fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()>;

    /// Processes outgoing write-buffer data.
    fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()>;

    /// Performs a graceful shutdown of the filter.
    fn shutdown(&self, ctx: &mut FilterCtx<'_>) -> io::Result<Poll<()>>;

    /// Checks whether read operations may proceed.
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness>;

    /// Checks whether write operations may proceed.
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

    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        let st = &self.0.0;
        if st.flags.is_closed() {
            Poll::Ready(Readiness::Terminate)
        } else {
            st.read_task.register(cx.waker());

            if st.flags.is_stopping_filters() {
                Poll::Ready(Readiness::Ready)
            } else if st.flags.is_read_paused_or_backpressure() {
                // read buffer is fulled of is not processed by dispatcher yet
                Poll::Pending
            } else {
                Poll::Ready(Readiness::Ready)
            }
        }
    }

    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        if self.0.0.flags.is_closed() {
            Poll::Ready(Readiness::Terminate)
        } else {
            self.0.0.write_task.register(cx.waker());

            if self.0.0.flags.is_stopping() {
                Poll::Ready(Readiness::Shutdown)
            } else if self.0.0.flags.is_write_paused() {
                Poll::Pending
            } else {
                Poll::Ready(Readiness::Ready)
            }
        }
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
                self.process_write_buf(ctx)?;
                self.2.set(true);

                // Discard the write buffer; it won't be processed
                ctx.clear_write_buf();
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
        self.1.poll_read_ready(cx)
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.1.poll_write_ready(cx)
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
