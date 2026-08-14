use std::{fmt, marker::PhantomData};

use super::{Service, ServiceCtx, ServiceFactory};

/// `MapInitErr` service combinator
pub struct MapInitErr<A, R, C, F, E> {
    a: A,
    f: F,
    e: PhantomData<fn(R, C) -> E>,
}

impl<A, R, C, F, E> MapInitErr<A, R, C, F, E>
where
    A: ServiceFactory<R, C>,
    F: Fn(A::Error) -> E,
{
    /// Create new `MapInitErr` combinator
    pub(crate) fn new(a: A, f: F) -> Self {
        Self {
            a,
            f,
            e: PhantomData,
        }
    }
}

impl<A, R, C, F, E> Clone for MapInitErr<A, R, C, F, E>
where
    A: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            f: self.f.clone(),
            e: PhantomData,
        }
    }
}

impl<A, R, C, F, E> fmt::Debug for MapInitErr<A, R, C, F, E>
where
    A: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapInitErr")
            .field("service", &self.a)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<A, R, C, F, E> Service<C> for MapInitErr<A, R, C, F, E>
where
    A: ServiceFactory<R, C>,
    A::Response: Service<R, Data = A::Data>,
    F: Fn(A::Error) -> E + Clone,
{
    type Response = A::Response;
    type Error = E;
    type Data = A::Data;

    #[inline]
    async fn call(
        &self,
        cfg: C,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.a, cfg, data).await.map_err(|e| (self.f)(e))
    }
}

#[cfg(test)]
mod tests {
    use crate::{chain_factory, fn_factory_with_config, fn_service};

    #[ntex::test]
    async fn map_init_err() {
        let factory = chain_factory(fn_factory_with_config(|err: &bool| {
            let err = *err;
            async move {
                if err {
                    Err(())
                } else {
                    Ok(fn_service::<_, _, _, _>(|i: usize| async move {
                        Ok::<_, ()>(i * 2)
                    }))
                }
            }
        }))
        .map_init_err(|()| std::io::Error::other("err"))
        .clone();

        assert!(factory.pipeline(&true, &()).await.is_err());
        assert!(factory.pipeline(&false, &()).await.is_ok());
        let _ = format!("{factory:?}");
    }

    #[ntex::test]
    async fn map_init_err2() {
        let factory = chain_factory(fn_factory_with_config(|err: &bool| {
            let err = *err;
            async move {
                if err {
                    Err(())
                } else {
                    Ok(fn_service::<_, _, _, _>(|i: usize| async move {
                        Ok::<_, ()>(i * 2)
                    }))
                }
            }
        }))
        .map_init_err(|()| std::io::Error::other("err"))
        .clone();

        assert!(factory.pipeline(&true, &()).await.is_err());
        assert!(factory.pipeline(&false, &()).await.is_ok());
        let _ = format!("{factory:?}");
    }
}
