use std::{any::Any, any::TypeId, fmt, io, ops, task::Context, task::Poll};

use crate::filter::{Filter, FilterReadStatus};
use crate::{buf::Stack, Io, IoRef, Readiness};

/// Sealed filter type
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
    fn process_read_buf(
        &self,
        io: &IoRef,
        stack: &Stack,
        idx: usize,
        nbytes: usize,
    ) -> io::Result<FilterReadStatus> {
        self.0.process_read_buf(io, stack, idx, nbytes)
    }

    #[inline]
    fn process_write_buf(&self, io: &IoRef, stack: &Stack, idx: usize) -> io::Result<()> {
        self.0.process_write_buf(io, stack, idx)
    }

    #[inline]
    fn shutdown(&self, io: &IoRef, stack: &Stack, idx: usize) -> io::Result<Poll<()>> {
        self.0.shutdown(io, stack, idx)
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
/// Boxed `Io` object with erased filter type
pub struct IoBoxed(Io<Sealed>);

impl IoBoxed {
    #[inline]
    /// Clone current io object.
    ///
    /// Current io object becomes closed.
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
