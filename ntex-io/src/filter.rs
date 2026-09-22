use std::{any, cell::Cell, io, task::Context, task::Poll};

use crate::{FilterCtx, FilterLayer, IoRef, Readiness};

#[derive(Debug)]
/// Base filter that connects a filter chain to the underlying transport.
pub struct Base(IoRef);

impl Base {
    pub(crate) fn new(inner: IoRef) -> Self {
        Base(inner)
    }
}

#[derive(Debug)]
/// One processing layer wrapped around an existing filter chain.
///
/// Values of this type are created by [`Io::add_filter`](crate::Io::add_filter).
/// `F` is the outer, newly added layer and `L` is the previously installed
/// inner chain.
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

/// Complete filter-chain interface used by [`Io`](crate::Io).
///
/// Most filters should implement [`FilterLayer`] and be installed with
/// [`Io::add_filter`](crate::Io::add_filter). Implement this trait directly
/// only when wrapping or replacing a complete chain.
pub trait Filter: 'static {
    /// Returns type-indexed information exposed by this chain.
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>>;

    /// Processes incoming data from the transport toward the application.
    fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()>;

    /// Processes outgoing data from the application toward the transport.
    fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()>;

    /// Performs graceful shutdown from the outermost layer toward the
    /// transport.
    fn shutdown(&self, ctx: &mut FilterCtx<'_>) -> io::Result<Poll<()>>;

    /// Checks whether transport read operations may proceed.
    ///
    /// Reads continue throughout a graceful shutdown, first so that filters can
    /// complete theirs, then to drain and discard whatever the peer still
    /// sends, so [`Readiness::Close`] is resolved only once the connection is
    /// terminated.
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness>;

    /// Checks whether transport write operations may proceed.
    ///
    /// Resolves to [`Readiness::Close`] once the connection is terminated, or
    /// once it enters a graceful shutdown and all buffered output has reached
    /// the transport.
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
        if st.flags.is_aborted() {
            Poll::Ready(Readiness::Close)
        } else {
            st.read_task.register(cx.waker());

            if st.flags.is_read_eof() {
                // The transport read side is closed, no further input can
                // arrive. This outranks filter shutdown below: a filter that
                // waits for input would otherwise keep the transport polling a
                // closed read side.
                Poll::Pending
            } else if st.flags.is_stopping() {
                // Transport shutdown phase. The filters are done, so incoming
                // data is read and discarded: it keeps the peer from stalling
                // and leaves the receive queue empty when the socket is closed,
                // which would otherwise abort the connection with an RST and
                // lose the output being drained.
                Poll::Ready(Readiness::Ready)
            } else if st.flags.is_stopping_filters() {
                // A filter may still need input to complete its shutdown, so
                // keep reading even though the application paused reads.
                Poll::Ready(Readiness::Ready)
            } else if st.flags.is_read_paused_or_backpressure() {
                // read buffer is full or is not processed by dispatcher yet
                Poll::Pending
            } else {
                Poll::Ready(Readiness::Ready)
            }
        }
    }

    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        let st = &self.0.0;
        if st.flags.is_aborted() {
            Poll::Ready(Readiness::Close)
        } else {
            st.write_task.register(cx.waker());

            if st.flags.is_stopping() {
                // Transport shutdown phase. Buffered output is drained into the
                // transport first; `Readiness::Close` is reported only once
                // nothing is left to write.
                if st.buffer.write_buf_size() != 0 {
                    Poll::Ready(Readiness::Ready)
                } else if st.wr_inflight.get() != 0 {
                    // the transport still holds output that has not reached
                    // the peer, its completion wakes the write task
                    Poll::Pending
                } else {
                    Poll::Ready(Readiness::Close)
                }
            } else if st.flags.is_write_paused() {
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
        Poll::Ready(Readiness::Close)
    }

    #[inline]
    fn poll_write_ready(&self, _: &mut Context<'_>) -> Poll<Readiness> {
        Poll::Ready(Readiness::Close)
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
