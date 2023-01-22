use std::ops;

use crate::{filter::FilterLayer, Io};

/// Sealed filter type
pub struct Sealed(pub(crate) Box<dyn FilterLayer>);

#[derive(Debug)]
/// Boxed `Io` object with erased filter type
pub struct IoBoxed(Io<Sealed>);

impl From<Io<Sealed>> for IoBoxed {
    fn from(io: Io<Sealed>) -> Self {
        Self(io)
    }
}

impl<F: FilterLayer> From<Io<F>> for IoBoxed {
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
