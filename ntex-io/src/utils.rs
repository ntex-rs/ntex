use std::{future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

use ntex_service::{fn_factory_with_config, into_service, Service, ServiceFactory};
use ntex_util::{future::Ready, ready};

pub use crate::framed::Framed;
pub use crate::io::OnDisconnect;
use crate::{Filter, FilterFactory, Io, IoBoxed};

/// Service that converts any Io<F> stream to IoBoxed stream
pub fn seal<F, S, C>(
    srv: S,
) -> impl ServiceFactory<
    Io<F>,
    C,
    Response = S::Response,
    Error = S::Error,
    InitError = S::InitError,
>
where
    F: Filter,
    S: ServiceFactory<IoBoxed, C>,
{
    fn_factory_with_config(move |cfg: C| {
        let fut = srv.new_service(cfg);
        async move {
            let srv = fut.await?;
            Ok(into_service(move |io: Io<F>| srv.call(IoBoxed::from(io))))
        }
    })
}

/// Service that converts Io<F> responses from service to the IoBoxed
pub fn boxed<S, R, F>(inner: S) -> Boxed<S, R>
where
    F: Filter,
    S: Service<R, Response = Io<F>>,
{
    Boxed {
        inner,
        _t: PhantomData,
    }
}

/// Create filter factory service
pub fn filter<T, F>(filter: T) -> FilterServiceFactory<T, F>
where
    T: FilterFactory<F> + Clone,
    F: Filter,
{
    FilterServiceFactory {
        filter,
        _t: PhantomData,
    }
}

pub struct BoxedFactory<S, R> {
    inner: S,
    _t: PhantomData<R>,
}

impl<S, R> BoxedFactory<S, R> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            _t: PhantomData,
        }
    }
}

impl<S: Clone, R> Clone for BoxedFactory<S, R> {
    fn clone(&self) -> Self {
        Self::new(self.inner.clone())
    }
}

impl<S, R, C, F> ServiceFactory<R, C> for BoxedFactory<S, R>
where
    F: Filter,
    S: ServiceFactory<R, C, Response = Io<F>>,
{
    type Response = IoBoxed;
    type Error = S::Error;
    type Service = Boxed<S::Service, R>;
    type InitError = S::InitError;
    type Future = BoxedFactoryResponse<S, R, C>;

    fn new_service(&self, cfg: C) -> Self::Future {
        BoxedFactoryResponse {
            fut: self.inner.new_service(cfg),
            _t: PhantomData,
        }
    }
}

pub struct Boxed<S, R> {
    inner: S,
    _t: PhantomData<R>,
}

impl<S, R> Boxed<S, R> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            _t: PhantomData,
        }
    }
}

impl<S: Clone, R> Clone for Boxed<S, R> {
    fn clone(&self) -> Self {
        Self::new(self.inner.clone())
    }
}

impl<S, R, F> Service<R> for Boxed<S, R>
where
    F: Filter,
    S: Service<R, Response = Io<F>>,
{
    type Response = IoBoxed;
    type Error = S::Error;
    type Future = BoxedResponse<S, R>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_err: bool) -> Poll<()> {
        self.inner.poll_shutdown(cx, is_err)
    }

    fn call(&self, req: R) -> Self::Future {
        BoxedResponse {
            fut: self.inner.call(req),
        }
    }
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    pub struct BoxedFactoryResponse<S: ServiceFactory<R, C>, R, C> {
        #[pin]
        fut: S::Future,
        _t: PhantomData<(R, C)>
    }
}

impl<S: ServiceFactory<R, C>, R, C> Future for BoxedFactoryResponse<S, R, C> {
    type Output = Result<Boxed<S::Service, R>, S::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(ready!(self.project().fut.poll(cx)).map(|inner| Boxed {
            inner,
            _t: PhantomData,
        }))
    }
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    pub struct BoxedResponse<S: Service<R>, R> {
        #[pin]
        fut: S::Future,
    }
}

impl<S: Service<R, Response = Io<F>>, R, F: Filter> Future for BoxedResponse<S, R> {
    type Output = Result<IoBoxed, S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(ready!(self.project().fut.poll(cx)).map(IoBoxed::from))
    }
}

pub struct FilterServiceFactory<T, F> {
    filter: T,
    _t: PhantomData<F>,
}

impl<T, F> ServiceFactory<Io<F>, ()> for FilterServiceFactory<T, F>
where
    T: FilterFactory<F> + Clone,
    F: Filter,
{
    type Response = Io<T::Filter>;
    type Error = T::Error;
    type Service = FilterService<T, F>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(FilterService {
            filter: self.filter.clone(),
            _t: PhantomData,
        })
    }
}

pub struct FilterService<T, F> {
    filter: T,
    _t: PhantomData<F>,
}

impl<T, F> Service<Io<F>> for FilterService<T, F>
where
    T: FilterFactory<F> + Clone,
    F: Filter,
{
    type Response = Io<T::Filter>;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, req: Io<F>) -> Self::Future {
        req.add_filter(self.filter.clone())
    }
}
