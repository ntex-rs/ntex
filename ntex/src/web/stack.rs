use std::{
    convert::Infallible, future::Future, marker::PhantomData, pin::Pin, task::Context,
    task::Poll,
};

use crate::service::{dev::AndThenFactory, Identity, Service, ServiceFactory, Transform};
use crate::util::{ready, Ready};

use super::{Error, ErrorRenderer, WebRequest, WebResponse, WebService, WebServiceConfig};

pub struct Stack<Inner, Outer> {
    inner: Inner,
    outer: Outer,
}

impl<Inner, Outer> Stack<Inner, Outer> {
    pub(super) fn new(inner: Inner, outer: Outer) -> Self {
        Stack { inner, outer }
    }
}

impl<S, Inner, Outer> Transform<S> for Stack<Inner, Outer>
where
    Inner: Transform<S>,
    Outer: Transform<Next<Inner::Service>>,
{
    type Service = Outer::Service;

    fn new_transform(&self, service: S) -> Self::Service {
        self.outer
            .new_transform(Next::new(self.inner.new_transform(service)))
    }
}

pub(super) struct MiddlewareStack<T, Err> {
    inner: T,
    _t: PhantomData<Err>,
}

impl<T, Err> MiddlewareStack<T, Err> {
    pub(super) fn new(inner: T) -> Self {
        Self {
            inner,
            _t: PhantomData,
        }
    }
}

impl<S, T, Err> Transform<S> for MiddlewareStack<T, Err>
where
    T: Transform<S>,
{
    type Service = Middleware<T::Service, Err>;

    fn new_transform(&self, service: S) -> Self::Service {
        Middleware {
            md: self.inner.new_transform(service),
            _t: PhantomData,
        }
    }
}

pub struct Middleware<S, Err> {
    md: S,
    _t: PhantomData<Err>,
}

impl<'a, S, Err> Service<&'a mut WebRequest<'a, Err>> for Middleware<S, Err>
where
    S: Service<&'a mut WebRequest<'a, Err>, Response = WebResponse, Error = Infallible>,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Infallible;
    type Future = MiddlewareResponse<'a, S, Err>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = ready!(self.md.poll_ready(cx));
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: &'a mut WebRequest<'a, Err>) -> Self::Future {
        MiddlewareResponse {
            fut: self.md.call(req),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct MiddlewareResponse<'a, S: Service<&'a mut WebRequest<'a, Err>>, Err> {
        #[pin]
        fut: S::Future,
    }
}

impl<'a, S, Err> Future for MiddlewareResponse<'a, S, Err>
where
    S: Service<&'a mut WebRequest<'a, Err>, Response = WebResponse, Error = Infallible>,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(ready!(self.project().fut.poll(cx)).unwrap()))
    }
}

pub struct Next<S> {
    next: S,
}

impl<S> Next<S> {
    pub(super) fn new(next: S) -> Self {
        Next { next }
    }
}

impl<'a, S, Err> Service<&'a mut WebRequest<'a, Err>> for Next<S>
where
    S: Service<&'a mut WebRequest<'a, Err>, Response = WebResponse> + 'static,
    S::Error: Error<Err>,
    S::Future: 'a,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Infallible;
    type Future = NextResponse<'a, S, Err>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = ready!(self.next.poll_ready(cx));
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: &'a mut WebRequest<'a, Err>) -> Self::Future {
        let r = unsafe { (req as *mut WebRequest<'a, Err>).as_mut().unwrap() };

        NextResponse {
            req,
            fut: self.next.call(r),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct NextResponse<'a, S: Service<&'a mut WebRequest<'a, Err>>, Err> {
        #[pin]
        fut: S::Future,
        req: &'a mut WebRequest<'a, Err>,
    }
}

impl<'a, S, Err> Future for NextResponse<'a, S, Err>
where
    S: Service<&'a mut WebRequest<'a, Err>, Response = WebResponse>,
    S::Error: Error<Err>,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match ready!(this.fut.poll(cx)) {
            Ok(res) => Poll::Ready(Ok(res)),
            Err(err) => {
                Poll::Ready(Ok(WebResponse::from_err(err, this.req.http_request())))
            }
        }
    }
}

pub struct Filter;

impl<'a, Err: ErrorRenderer> FiltersFactory<'a, Err> for Filter {
    type Service = Filter;

    fn create(self) -> Self::Service {
        self
    }
}

impl<'a, Err: ErrorRenderer> ServiceFactory<&'a mut WebRequest<'a, Err>> for Filter {
    type Response = &'a mut WebRequest<'a, Err>;
    type Error = Infallible;
    type InitError = ();
    type Service = Filter;
    type Future = Ready<Filter, ()>;

    #[inline]
    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(Filter)
    }
}

impl<'a, Err: ErrorRenderer> Service<&'a mut WebRequest<'a, Err>> for Filter {
    type Response = &'a mut WebRequest<'a, Err>;
    type Error = Infallible;
    type Future = Ready<Self::Response, Self::Error>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: &'a mut WebRequest<'a, Err>) -> Self::Future {
        Ready::Ok(req)
    }
}

pub struct Filters<First, Second> {
    first: First,
    second: Second,
}

impl<First, Second> Filters<First, Second> {
    pub(super) fn new(first: First, second: Second) -> Self {
        Filters { first, second }
    }
}

impl<'a, First, Second, Err> FiltersFactory<'a, Err> for Filters<First, Second>
where
    Err: ErrorRenderer,
    First: ServiceFactory<
            &'a mut WebRequest<'a, Err>,
            Response = &'a mut WebRequest<'a, Err>,
            Error = Infallible,
            InitError = (),
        > + 'static,
    First::Service: 'static,
    First::Future: 'static,
    <First::Service as Service<&'a mut WebRequest<'a, Err>>>::Future: 'a,
    Second: FiltersFactory<'a, Err>,
    Second::Service: ServiceFactory<
        &'a mut WebRequest<'a, Err>,
        Response = &'a mut WebRequest<'a, Err>,
        Error = Infallible,
        InitError = (),
    >,
    <Second::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service: 'static,
    <Second::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Future: 'static,
   <<Second::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service as Service<&'a mut WebRequest<'a, Err>>>::Future: 'a,
{
    type Service = AndThenFactory<First, Second::Service>;

    fn create(self) -> Self::Service {
        AndThenFactory::new(self.first, self.second.create())
    }
}

pub trait FiltersFactory<'a, Err: ErrorRenderer> {
    type Service: ServiceFactory<
            &'a mut WebRequest<'a, Err>,
            Response = &'a mut WebRequest<'a, Err>,
            Error = Infallible,
            InitError = (),
        > + 'static;

    fn create(self) -> Self::Service;
}

impl<'a, Err: ErrorRenderer> ServicesFactory<'a, Err> for Identity {
    fn register(self, _: &mut WebServiceConfig<'a, Err>) {}
}

pub struct Services<First, Second> {
    first: First,
    second: Second,
}

impl<First, Second> Services<First, Second> {
    pub(super) fn new(first: First, second: Second) -> Self {
        Services { first, second }
    }
}

impl<'a, First, Second, Err> ServicesFactory<'a, Err> for Services<First, Second>
where
    Err: ErrorRenderer,
    First: WebService<'a, Err>,
    Second: ServicesFactory<'a, Err>,
{
    fn register(self, cfg: &mut WebServiceConfig<'a, Err>) {
        self.second.register(cfg);
        self.first.register(cfg);
    }
}

pub trait ServicesFactory<'a, Err: ErrorRenderer> {
    fn register(self, cfg: &mut WebServiceConfig<'a, Err>);
}
