use crate::{Ctx, IntoService, Service};

/// Create `map state` service
pub fn map_state<S, St, Req>(st: St, s: impl IntoService<S, St, Req>) -> MapState<S, St>
where
    S: Service<St, Req>,
{
    MapState::new(s.into_service(), st)
}

#[derive(Clone, Debug)]
/// Map state for inner service
pub struct MapState<S, St> {
    s: S,
    st: St,
}

impl<S, St> MapState<S, St> {
    /// Create new `MapState` instance
    pub fn new<Req>(s: S, st: St) -> Self
    where
        S: Service<St, Req>,
    {
        Self { s, st }
    }
}

impl<OtSt, S, St, Req> Service<OtSt, Req> for MapState<S, St>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, OtSt>) -> Result<S::Res, S::Error> {
        ctx.map_state(&self.st).call(&self.s, req).await
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, OtSt>) -> Result<(), S::Error> {
        ctx.map_state(&self.st).ready(&self.s).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, OtSt>) {
        ctx.map_state(&self.st).shutdown(&self.s).await;
    }
}
