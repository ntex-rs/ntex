use std::{any::Any, any::TypeId, fmt, io, ops, task::Context, task::Poll};

use crate::{Filter, FilterCtx, Io, Readiness};

/// Type-erased filter chain used by [`IoBoxed`].
pub struct Sealed(pub(crate) Box<dyn Filter>);

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed").finish()
    }
}

impl Filter for Sealed {
    #[inline]
    fn query(&self, id: TypeId) -> Option<Box<dyn Any>> {
        self.0.query(id)
    }

    #[inline]
    fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
        self.0.process_read_buf(ctx)
    }

    #[inline]
    fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
        self.0.process_write_buf(ctx)
    }

    #[inline]
    fn shutdown(&self, ctx: &mut FilterCtx<'_>) -> io::Result<Poll<()>> {
        self.0.shutdown(ctx)
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.0.poll_read_ready(cx)
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.0.poll_write_ready(cx)
    }
}

#[derive(Debug)]
/// An [`Io`] object whose filter-chain type has been erased.
pub struct IoBoxed(Io<Sealed>);

impl IoBoxed {
    #[inline]
    #[must_use]
    /// Transfers the live I/O state into a new object.
    ///
    /// This does not clone the connection. The current object is replaced with
    /// a stopped placeholder and should no longer be used for I/O.
    pub fn take(&mut self) -> Self {
        IoBoxed(self.0.take())
    }
}

impl<F: Filter> From<Io<F>> for IoBoxed {
    fn from(io: Io<F>) -> Self {
        Self(io.seal())
    }
}

impl ops::Deref for IoBoxed {
    type Target = Io<Sealed>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<IoBoxed> for Io<Sealed> {
    fn from(value: IoBoxed) -> Self {
        value.0
    }
}
