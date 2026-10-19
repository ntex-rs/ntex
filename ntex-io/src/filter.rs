use std::{any, cell::Cell, io, task::Context, task::Poll};

use crate::{FilterCtx, FilterLayer, IoRef, Readiness, io::IoState};

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

    /// Checks whether the chain is ready to process more input.
    ///
    /// This is polled by the transport read task only, a chain that is not
    /// ready pauses transport reads until it wakes the task given in `cx`. The
    /// dispatcher is notified when the pause starts and when it ends, see
    /// [`IoRef::is_filter_paused`](crate::IoRef::is_filter_paused). Once the
    /// chain is ready again, input and output it holds are processed again
    /// without waiting for new transport input. A chain that stops being ready
    /// between polls is only observed by the next poll, it can wake the task
    /// given to the previous one.
    ///
    /// Transport writes are not paused, output that reached the transport is
    /// always delivered. A filter that is not ready to produce output keeps it
    /// in its own buffers instead, see
    /// [`FilterLayer::process_write_buf`]. A filter that waits for input from
    /// the peer, for example during a TLS handshake, must not report `Pending`
    /// here, that input would never be read.
    ///
    /// Graceful shutdown and its deadline do not depend on the chain being
    /// ready. An error ends the connection like any filter error: the chain
    /// is not called again and its shutdown is skipped, output that reached
    /// the transport is still delivered within the shutdown deadline.
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

/// Transport read readiness as decided by the io state, without the filter
/// chain.
pub(crate) fn read_readiness(st: &IoState) -> Poll<Readiness> {
    if st.flags.is_force_closing() {
        // Only an explicit `IoRef::terminate()` aborts the connection. A
        // transport failure or an expired shutdown deadline end the
        // connection too, but the transport still closes it gracefully.
        Poll::Ready(Readiness::Terminate)
    } else if st.flags.is_aborted() {
        // The connection ended because of a failure, so no further input
        // can be used; the transport closes it gracefully.
        Poll::Ready(Readiness::Close)
    } else if st.flags.is_read_eof() {
        // The transport read side is closed, no further input can
        // arrive. This outranks filter shutdown below: a filter that
        // waits for input would otherwise keep the transport polling a
        // closed read side.
        Poll::Pending
    } else if st.flags.is_stopping() {
        // Transport shutdown phase. The filters are done, so no further
        // input can be used and the read task pauses. The receive queue
        // is drained by the transport itself, just before it closes the
        // connection.
        Poll::Pending
    } else if st.flags.is_stopping_filters() {
        // A filter may still need input to complete its shutdown, so
        // keep reading even though the application paused reads.
        Poll::Ready(Readiness::Ready)
    } else if st.flags.is_read_paused_or_backpressure() || st.flags.is_read_wr_backpressure() {
        // read buffer is full or is not processed by dispatcher yet,
        // or output produced by reading has not drained
        Poll::Pending
    } else {
        Poll::Ready(Readiness::Ready)
    }
}

/// Transport write readiness as decided by the io state, without the filter
/// chain.
pub(crate) fn write_readiness(st: &IoState) -> Poll<Readiness> {
    if st.flags.is_force_closing() {
        // see `read_readiness`
        Poll::Ready(Readiness::Terminate)
    } else if st.flags.is_aborted() {
        // The connection ended because of a failure, so there is nothing
        // left to drain; the transport closes it gracefully.
        Poll::Ready(Readiness::Close)
    } else if st.flags.is_stopping() {
        // Transport shutdown phase. Buffered output is drained into the
        // transport first; `Readiness::Close` is reported only once
        // nothing is left to write. Output a filter holds back pauses the
        // write task, it waits for the shutdown deadline then.
        if st.buffer.write_dst_size() != 0
            || (st.buffer.write_buf_size() != 0 && !st.flags.is_write_paused())
        {
            Poll::Ready(Readiness::Ready)
        } else if st.buffer.write_buf_size() != 0 || st.wr_inflight.get() != 0 {
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

impl Filter for Base {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if let Some(hnd) = self.0.0.handle.take() {
            let res = hnd.query(id);
            self.0.restore_handle(hnd);
            res
        } else {
            None
        }
    }

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
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
                // Output produced by this filter's shutdown sits in the buffer
                // of the inner filter, which only runs its write processing
                // here, so move it towards the transport while waiting.
                ctx.with_next(|ctx| self.1.process_write_buf(ctx))?;
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
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.1.poll_ready(cx)
    }
}

impl Filter for NullFilter {
    #[inline]
    fn query(&self, _: any::TypeId) -> Option<Box<dyn any::Any>> {
        None
    }

    // The filter chain is gone once `Io` has been dropped, `IoContext`
    // reports the connection closed then and does not poll the chain.
    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
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
