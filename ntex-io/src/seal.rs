use std::{fmt, ops};

use crate::{filter::Filter, Io};

/// Sealed filter type
pub struct Sealed(pub(crate) Box<dyn Filter>);

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed").finish()
    }
}

#[derive(Debug)]
/// Boxed `Io` object with erased filter type
pub struct IoBoxed(Io<Sealed>);

impl From<Io<Sealed>> for IoBoxed {
    fn from(io: Io<Sealed>) -> Self {
        Self(io)
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
