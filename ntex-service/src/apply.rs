#![allow(clippy::type_complexity)]
use std::{future::Future, marker, pin::Pin, task, task::Poll};

use super::ctx::{ServiceCall, ServiceCtx};
use super::{IntoService, IntoServiceFactory, Pipeline, Service, ServiceFactory};

/// Apply transform function to a service.
pub fn apply_fn<T, Req, F, R, In, Out, Err, U>(
    service: U,
    f: F,
) -> Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req, Error = Err>,
    F: Fn(In, ApplyService<T>) -> R,
    R: Future<Output = Result<Out, Err>>,
    U: IntoService<T, Req>,
{
    Apply {
        f,
        service: Pipeline::new(service.into_service()),
        r: marker::PhantomData,
    }
}

/// Service factory that produces `apply_fn` service.
pub fn apply_fn_factory<T, Req, Cfg, F, R, In, Out, Err, U>(
    service: U,
    f: F,
) -> ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg, Error = Err>,
    F: Fn(In, ApplyService<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    U: IntoServiceFactory<T, Req, Cfg>,
{
    ApplyFactory::new(service.into_factory(), f)
}

/// `Apply` service combinator
pub struct Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req, Error = Err>,
{
    service: Pipeline<T>,
    f: F,
    r: marker::PhantomData<fn(Req) -> (In, Out, R)>,
}

pub struct ApplyService<S> {
    service: Pipeline<S>,
}

impl<S> ApplyService<S> {
    /// Check readiness and call enclosed service.
    pub fn call<R>(&self, req: R) -> ServiceCall<'_, S, R>
    where
        S: Service<R>,
    {
        self.service.service_call(req)
    }
}

impl<T, Req, F, R, In, Out, Err> Clone for Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req, Error = Err> + Clone,
    F: Fn(In, ApplyService<T>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
{
    fn clone(&self) -> Self {
        Apply {
            service: self.service.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, F, R, In, Out, Err> Service<In> for Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req, Error = Err>,
    F: Fn(In, ApplyService<T>) -> R,
    R: Future<Output = Result<Out, Err>>,
{
    type Response = Out;
    type Error = Err;
    type Future<'f> = R where Self: 'f, In: 'f, R: 'f;

    crate::forward_poll_ready!(service);
    crate::forward_poll_shutdown!(service);

    #[inline]
    fn call<'a>(&'a self, req: In, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        let svc = ApplyService {
            service: self.service.clone(),
        };
        (self.f)(req, svc)
    }
}

/// `apply()` service factory
pub struct ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg, Error = Err>,
    F: Fn(In, ApplyService<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
{
    service: T,
    f: F,
    r: marker::PhantomData<fn(Req, Cfg) -> (R, In, Out)>,
}

impl<T, Req, Cfg, F, R, In, Out, Err> ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg, Error = Err>,
    F: Fn(In, ApplyService<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
{
    /// Create new `ApplyNewService` new service instance
    fn new(service: T, f: F) -> Self {
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
    T: ServiceFactory<Req, Cfg, Error = Err> + Clone,
    F: Fn(In, ApplyService<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
{
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, Cfg, F, R, In, Out, Err> ServiceFactory<In, Cfg>
    for ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg, Error = Err>,
    F: Fn(In, ApplyService<T::Service>) -> R + Clone,
    for<'r> R: Future<Output = Result<Out, Err>> + 'r,
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
        T: ServiceFactory<Req, Cfg, Error = Err>,
        T: 'f,
        F: Fn(In, ApplyService<T::Service>) -> R,
        T::Service: 'f,
        R: Future<Output = Result<Out, Err>>,
        Cfg: 'f,
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
    T: ServiceFactory<Req, Cfg, Error = Err>,
    F: Fn(In, ApplyService<T::Service>) -> R,
    R: Future<Output = Result<Out, Err>>,
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

    #[derive(Clone)]
    struct Srv;

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();
        type Future<'f> = Ready<(), ()>;

        fn call<'a>(&'a self, _: (), _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
            Ready::Ok(())
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

        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
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

        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
    }
}
