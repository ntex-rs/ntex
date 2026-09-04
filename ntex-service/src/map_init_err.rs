use std::{fmt, marker::PhantomData};

use super::ServiceFactory;

/// `MapInitError` service combinator
pub struct MapInitErr<F, Sf, Err> {
    f: F,
    sf: Sf,
    e: PhantomData<fn() -> Err>,
}

impl<F, Sf, Err> MapInitErr<F, Sf, Err> {
    /// Create new `MapInitErr` combinator
    pub(crate) fn new<St, Req, Cfg>(f: F, sf: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Cfg>,
        F: Fn(Sf::InitError) -> Err,
    {
        Self {
            f,
            sf,
            e: PhantomData,
        }
    }
}

impl<F, Sf, Err> Clone for MapInitErr<F, Sf, Err>
where
    F: Clone,
    Sf: Clone,
{
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            sf: self.sf.clone(),
            e: PhantomData,
        }
    }
}

impl<F, Sf, Err> fmt::Debug for MapInitErr<F, Sf, Err>
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

impl<F, Sf, St, Req, Cfg, Err> ServiceFactory<St, Req, Cfg> for MapInitErr<F, Sf, Err>
where
    F: Fn(Sf::InitError) -> Err + Clone,
    Sf: ServiceFactory<St, Req, Cfg>,
{
    type Res = Sf::Res;
    type Error = Sf::Error;

    type Service = Sf::Service;
    type InitError = Err;

    #[inline]
    async fn create(&self, cfg: &Cfg) -> Result<Self::Service, Self::InitError> {
        self.sf.create(cfg).await.map_err(|e| (self.f)(e))
    }
}

#[cfg(test)]
mod tests {
    use crate::{ServiceFactory, factory_no_st, fn_factory, fn_service};

    #[ntex::test]
    async fn map_init_err() {
        let factory = factory_no_st(fn_factory(async move |err: &bool| {
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
        let factory = factory_no_st(fn_factory(async |err: &bool| {
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
