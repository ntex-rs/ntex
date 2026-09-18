use crate::{Ctx, IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Wraps a service with a fixed state value.
///
/// The wrapped service uses `st` instead of the state from its outer pipeline.
pub fn map_state<S, St, Req>(st: St, s: impl IntoService<S, St, Req>) -> MapState<S, St>
where
    S: Service<St, Req>,
{
    MapState {
        st,
        s: s.into_service(),
    }
}

/// Wraps a service factory with a fixed state value.
///
/// The fixed state is used both to create services and to process their calls.
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
/// A service that substitutes fixed state for the outer pipeline state.
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
/// A factory that creates [`MapState`] services using fixed state.
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

#[cfg(test)]
mod tests {
    use crate::{Pipeline, ServiceFactory, fn_service_st, map_state, map_state_factory};

    #[ntex::test]
    async fn test_map_state() {
        let svc = map_state(
            100,
            fn_service_st(|_: &usize, item: usize| async move { Ok::<_, ()>(item) }),
        )
        .clone();
        let _ = format!("{svc:?}");

        let svc = Pipeline::new((), svc);
        assert_eq!(svc.call(1).await.unwrap(), 1);
        assert!(!svc.is_shutdown());
        svc.shutdown().await;
        assert!(svc.is_shutdown());

        let factory = map_state_factory(
            100,
            fn_service_st(|_: &usize, item: usize| async move { Ok::<_, ()>(item) }),
        )
        .clone();
        let _ = format!("{factory:?}");

        let svc = Pipeline::new((), factory.create(&1).await.unwrap());
        assert_eq!(svc.call(1).await.unwrap(), 1);
    }
}
