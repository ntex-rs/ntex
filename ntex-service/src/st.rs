use std::{fmt, sync::Arc};

/// Trait for types that can serve as pipeline state.
pub trait State<Req>: Sized + 'static {
    /// Updates the state in response to a request.
    #[inline]
    fn on_req(&self, _: &Req) -> Option<Self> {
        None
    }
}

pub trait FromState<St>: Sized {
    fn from(st: &St) -> Self;
}

impl<Req> State<Req> for () {}

impl<St> FromState<St> for () {
    fn from(_: &St) {}
}

/// State mapping
pub struct StateMapping<St, Chained> {
    f: Arc<dyn Fn(&Chained) -> St>,
}

impl<St, Chained> StateMapping<St, Chained> {
    pub fn state(&self, st: &Chained) -> St {
        (self.f)(st)
    }

    pub fn from_st() -> Self
    where
        St: FromState<Chained>,
    {
        Self {
            f: Arc::new(|st| St::from(st)),
        }
    }

    pub fn from_fn<F>(f: F) -> Self
    where
        F: Fn(&Chained) -> St + Send + Sync + 'static,
    {
        Self { f: Arc::from(f) }
    }
}

impl<St, Chained> Default for StateMapping<St, Chained>
where
    St: Default + 'static,
{
    fn default() -> Self {
        Self {
            f: Arc::new(|_| St::default()),
        }
    }
}

impl<St, Chained> Clone for StateMapping<St, Chained> {
    fn clone(&self) -> Self {
        Self { f: self.f.clone() }
    }
}

impl<St, Chained> fmt::Debug for StateMapping<St, Chained> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StateMapping").finish()
    }
}

// SAFETY: Send cannot be provided authomatically because of St and Chained params
// but code get executed in one thread and never leave it
unsafe impl<St, Chained> Send for StateMapping<St, Chained> {}
