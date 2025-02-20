use std::{fmt, ops};

use crate::{filter::Filter, Io};

/// Sealed filter type
pub struct Sealed(pub(crate) Box<dyn Filter>);

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed").finish()
    }
}

impl Filter for Sealed {
    fn query(&self, id: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
        self.0.query(id)
    }

    fn process_read_buf(
        &self,
        io: &crate::IoRef,
        stack: &crate::buf::Stack,
        idx: usize,
        nbytes: usize,
    ) -> std::io::Result<crate::filter::FilterReadStatus> {
        self.0.process_read_buf(io, stack, idx, nbytes)
    }

    fn process_write_buf(
        &self,
        io: &crate::IoRef,
        stack: &crate::buf::Stack,
        idx: usize,
    ) -> std::io::Result<()> {
        self.0.process_write_buf(io, stack, idx)
    }

    fn shutdown(
        &self,
        io: &crate::IoRef,
        stack: &crate::buf::Stack,
        idx: usize,
    ) -> std::io::Result<std::task::Poll<()>> {
        self.0.shutdown(io, stack, idx)
    }

    fn poll_read_ready(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<crate::ReadStatus> {
        self.0.poll_read_ready(cx)
    }

    fn poll_write_ready(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<crate::WriteStatus> {
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
