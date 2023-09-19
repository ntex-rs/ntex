#![allow(clippy::type_complexity)]
use std::{fmt, future::Future, marker, pin::Pin, task, task::Poll};

use super::ctx::ServiceCtx;
use super::{IntoService, IntoServiceFactory, Pipeline, Service, ServiceFactory};

/// Apply transform function to a service.
pub fn apply_fn<T, Req, F, R, In, Out, Err, U>(
    service: U,
    f: F,
) -> Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req>,
    F: Fn(In, Pipeline<T>) -> R,
    R: Future<Output = Result<Out, Err>>,
    U: IntoService<T, Req>,
    Err: From<T::Error>,
{
    Apply::new(service.into_service(), f)
}

/// Service factory that produces `apply_fn` service.
pub fn apply_fn_factory<T, Req, Cfg, F, R, In, Out, Err, U>(
    service: U,
    f: F,
) -> ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    U: IntoServiceFactory<T, Req, Cfg>,
    Err: From<T::Error>,
{
    ApplyFactory::new(service.into_factory(), f)
}

/// `Apply` service combinator
pub struct Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req>,
{
    service: Pipeline<T>,
    f: F,
    r: marker::PhantomData<fn(Req) -> (In, Out, R, Err)>,
}

impl<T, Req, F, R, In, Out, Err> Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req>,
    F: Fn(In, Pipeline<T>) -> R,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    pub(crate) fn new(service: T, f: F) -> Self {
        Apply {
            f,
            service: Pipeline::new(service),
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, F, R, In, Out, Err> Clone for Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req> + Clone,
    F: Fn(In, Pipeline<T>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    fn clone(&self) -> Self {
        Apply {
            service: self.service.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, F, R, In, Out, Err> fmt::Debug for Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Apply")
            .field("service", &self.service)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<T, Req, F, R, In, Out, Err> Service<In> for Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req>,
    F: Fn(In, Pipeline<T>) -> R,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    type Response = Out;
    type Error = Err;
    type Future<'f> = R where Self: 'f, In: 'f, R: 'f;

    crate::forward_poll_ready!(service);
    crate::forward_poll_shutdown!(service);

    #[inline]
    fn call<'a>(&'a self, req: In, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        (self.f)(req, self.service.clone())
    }
}

/// `apply()` service factory
pub struct ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
{
    service: T,
    f: F,
    r: marker::PhantomData<fn(Req, Cfg) -> (R, In, Out)>,
}

impl<T, Req, Cfg, F, R, In, Out, Err> ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    /// Create new `ApplyNewService` new service instance
    pub(crate) fn new(service: T, f: F) -> Self {
        Self {
            f,
            service,
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, Cfg, F, R, In, Out, Err> Clone
    for ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg> + Clone,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, Cfg, F, R, In, Out, Err> fmt::Debug
    for ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg> + fmt::Debug,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyFactory")
            .field("factory", &self.service)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<T, Req, Cfg, F, R, In, Out, Err> ServiceFactory<In, Cfg>
    for ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    type Response = Out;
    type Error = Err;

    type Service = Apply<T::Service, Req, F, R, In, Out, Err>;
    type InitError = T::InitError;
    type Future<'f> = ApplyFactoryResponse<'f, T, Req, Cfg, F, R, In, Out, Err> where Self: 'f, Cfg: 'f;

    #[inline]
    fn create(&self, cfg: Cfg) -> Self::Future<'_> {
        ApplyFactoryResponse {
            fut: self.service.create(cfg),
            f: Some(self.f.clone()),
            _t: marker::PhantomData,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct ApplyFactoryResponse<'f, T, Req, Cfg, F, R, In, Out, Err>
    where
        T: ServiceFactory<Req, Cfg>,
        T: 'f,
        F: Fn(In, Pipeline<T::Service>) -> R,
        T::Service: 'f,
        R: Future<Output = Result<Out, Err>>,
        Cfg: 'f,
        Err: From<T::Error>,
    {
        #[pin]
        fut: T::Future<'f>,
        f: Option<F>,
        _t: marker::PhantomData<(In, Out)>,
    }
}

impl<'f, T, Req, Cfg, F, R, In, Out, Err> Future
    for ApplyFactoryResponse<'f, T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    F: Fn(In, Pipeline<T::Service>) -> R,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    type Output = Result<Apply<T::Service, Req, F, R, In, Out, Err>, T::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if let Poll::Ready(svc) = this.fut.poll(cx)? {
            Poll::Ready(Ok(Apply {
                service: svc.into(),
                f: this.f.take().unwrap(),
                r: marker::PhantomData,
            }))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::{lazy, Ready};
    use std::task::Poll;

    use super::*;
    use crate::{chain, chain_factory, Service, ServiceCtx};

    #[derive(Clone, Debug)]
    struct Srv;

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();
        type Future<'f> = Ready<(), ()>;

        fn call<'a>(&'a self, _: (), _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
            Ready::Ok(())
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    struct Err;

    impl From<()> for Err {
        fn from(_: ()) -> Self {
            Err
        }
    }

    #[ntex::test]
    async fn test_call() {
        let srv = chain(
            apply_fn(Srv, |req: &'static str, svc| async move {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone(),
        )
        .pipeline();

        assert_eq!(
            lazy(|cx| srv.poll_ready(cx)).await,
            Poll::Ready(Ok::<_, Err>(()))
        );
        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
    }

    #[ntex::test]
    async fn test_call_chain() {
        let srv = chain(Srv)
            .apply_fn(|req: &'static str, svc| async move {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone()
            .pipeline();

        assert_eq!(
            lazy(|cx| srv.poll_ready(cx)).await,
            Poll::Ready(Ok::<_, Err>(()))
        );
        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        format!("{:?}", srv);
    }

    #[ntex::test]
    async fn test_create() {
        let new_srv = chain_factory(
            apply_fn_factory(
                || Ready::<_, ()>::Ok(Srv),
                |req: &'static str, srv| async move {
                    srv.call(()).await.unwrap();
                    Ok((req, ()))
                },
            )
            .clone(),
        );

        let srv = new_srv.pipeline(&()).await.unwrap();

        assert_eq!(
            lazy(|cx| srv.poll_ready(cx)).await,
            Poll::Ready(Ok::<_, Err>(()))
        );

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        format!("{:?}", new_srv);
    }

    #[ntex::test]
    async fn test_create_chain() {
        let new_srv = chain_factory(|| Ready::<_, ()>::Ok(Srv))
            .apply_fn(|req: &'static str, srv| async move {
                srv.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone();

        let srv = new_srv.pipeline(&()).await.unwrap();

        assert_eq!(
            lazy(|cx| srv.poll_ready(cx)).await,
            Poll::Ready(Ok::<_, Err>(()))
        );

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        format!("{:?}", new_srv);
    }
}
