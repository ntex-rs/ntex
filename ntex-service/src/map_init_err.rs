use std::{fmt, marker::PhantomData};

use super::ServiceFactory;

/// `MapInitError` service combinator
pub struct MapInitErr<Sf, F, Err> {
    sf: Sf,
    f: F,
    e: PhantomData<fn() -> Err>,
}

impl<Sf, F, Err> MapInitErr<Sf, F, Err> {
    /// Create new `MapInitErr` combinator
    pub(crate) fn new<Req, St>(sf: Sf, f: F) -> Self
    where
        Sf: ServiceFactory<Req, St>,
        F: Fn(Sf::InitError) -> Err,
    {
        Self {
            sf,
            f,
            e: PhantomData,
        }
    }
}

impl<Sf, F, Err> Clone for MapInitErr<Sf, F, Err>
where
    Sf: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            sf: self.sf.clone(),
            f: self.f.clone(),
            e: PhantomData,
        }
    }
}

impl<Sf, F, Err> fmt::Debug for MapInitErr<Sf, F, Err>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapInitErr")
            .field("sf", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<Sf, St, Req, F, Err> ServiceFactory<Req, St> for MapInitErr<Sf, F, Err>
where
    Sf: ServiceFactory<Req, St>,
    F: Fn(Sf::InitError) -> Err + Clone,
{
    type Res = Sf::Res;
    type Error = Sf::Error;

    type Service = Sf::Service;
    type InitCfg = Sf::InitCfg;
    type InitError = Err;

    #[inline]
    async fn create(&self, cfg: &Sf::InitCfg) -> Result<Self::Service, Self::InitError> {
        self.sf.create(cfg).await.map_err(|e| (self.f)(e))
    }
}

#[cfg(test)]
mod tests {
    use crate::{ServiceFactory, factory, fn_factory_with_config, fn_service};

    #[ntex::test]
    async fn map_init_err() {
        let factory = factory(fn_factory_with_config(async move |err: &bool| {
            if *err {
                Err(())
            } else {
                Ok(fn_service(async |i: usize| Ok::<_, ()>(i * 2)))
            }
        }))
        .map_init_err(|()| std::io::Error::other("err"))
        .clone();

        assert!(factory.create(&true).await.is_err());
        assert!(factory.create(&false).await.is_ok());
        let _ = format!("{factory:?}");
    }

    #[ntex::test]
    async fn map_init_err2() {
        let factory = factory(fn_factory_with_config(async |err: &bool| {
            if *err {
                Err(())
            } else {
                Ok(fn_service(async |i: usize| Ok::<_, ()>(i * 2)))
            }
        }))
        .map_init_err(|()| std::io::Error::other("err"))
        .clone();

        assert!(factory.create(&true).await.is_err());
        assert!(factory.create(&false).await.is_ok());
        let _ = format!("{factory:?}");
    }
}
