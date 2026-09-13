use crate::{Ctx, IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Create `map state` service
pub fn map_state<S, St, Req>(st: St, s: impl IntoService<S, St, Req>) -> MapState<S, St>
where
    S: Service<St, Req>,
{
    MapState {
        st,
        s: s.into_service(),
    }
}

/// Create `map state` service factory
pub fn map_state_factory<Sf, St, Req>(
    st: St,
    sf: impl IntoServiceFactory<Sf, St, Req>,
) -> MapStateFactory<Sf, St>
where
    Sf: ServiceFactory<St, Req>,
    St: Clone,
{
    MapStateFactory {
        st,
        sf: sf.into_factory(),
    }
}

#[derive(Clone, Debug)]
/// Map state for inner service
pub struct MapState<S, St> {
    s: S,
    st: St,
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

#[derive(Clone, Debug)]
/// Factory for map state for inner service
pub struct MapStateFactory<Sf, St> {
    sf: Sf,
    st: St,
}

impl<OtSt, Sf, St, Req> ServiceFactory<OtSt, Req> for MapStateFactory<Sf, St>
where
    Sf: ServiceFactory<St, Req>,
    St: Clone,
{
    type Res = Sf::Res;
    type Error = Sf::Error;

    type Service = MapState<Sf::Service, St>;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, _: &OtSt) -> Result<Self::Service, Self::InitError> {
        Ok(MapState {
            s: self.sf.create(&self.st).await?,
            st: self.st.clone(),
        })
    }
}
