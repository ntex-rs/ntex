use std::{
    convert::Infallible, future::Future, marker::PhantomData, pin::Pin, task::Context,
    task::Poll,
};

use crate::service::{Service, ServiceFactory, Transform};
use crate::util::{ready, Ready};

use super::httprequest::{HttpRequest, WeakHttpRequest};
use super::{ErrorContainer, ErrorRenderer, WebRequest, WebResponse};

pub struct Stack<Inner, Outer, Err> {
    inner: Inner,
    outer: Outer,
    _t: PhantomData<Err>,
}

impl<Inner, Outer, Err> Stack<Inner, Outer, Err> {
    pub(super) fn new(inner: Inner, outer: Outer) -> Self {
        Stack {
            inner,
            outer,
            _t: PhantomData,
        }
    }
}

impl<S, Inner, Outer, Err> Transform<S> for Stack<Inner, Outer, Err>
where
    Err: ErrorRenderer,
    Inner: Transform<S>,
    Inner::Service: Service<WebRequest<Err>, Response = WebResponse>,
    <Inner::Service as Service<WebRequest<Err>>>::Error: Into<Err::Container>,
    Outer: Transform<Next<Inner::Service, Err>>,
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
    T: Transform<Next<S, Err>>,
    S: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Service = Middleware<T::Service, Err>;

    fn new_transform(&self, service: S) -> Self::Service {
        Middleware {
            md: self.inner.new_transform(Next::new(service)),
            _t: PhantomData,
        }
    }
}

pub struct Middleware<S, Err> {
    md: S,
    _t: PhantomData<Err>,
}

impl<S, Err> Service<WebRequest<Err>> for Middleware<S, Err>
where
    S: Service<WebRequest<Err>, Response = WebResponse, Error = Infallible>,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = MiddlewareResponse<S, Err>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = ready!(self.md.poll_ready(cx));
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: WebRequest<Err>) -> Self::Future {
        MiddlewareResponse {
            fut: self.md.call(req),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct MiddlewareResponse<S: Service<WebRequest<Err>>, Err> {
        #[pin]
        fut: S::Future,
    }
}

impl<S, Err> Future for MiddlewareResponse<S, Err>
where
    S: Service<WebRequest<Err>, Response = WebResponse, Error = Infallible>,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Err::Container>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(ready!(self.project().fut.poll(cx)).unwrap()))
    }
}

pub struct Next<S, Err> {
    inner: S,
    _t: PhantomData<Err>,
}

impl<S, Err> Next<S, Err>
where
    S: Service<WebRequest<Err>, Response = WebResponse>,
    S::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    pub(super) fn new(inner: S) -> Self {
        Next {
            inner,
            _t: PhantomData,
        }
    }
}

impl<S, Err> Service<WebRequest<Err>> for Next<S, Err>
where
    S: Service<WebRequest<Err>, Response = WebResponse>,
    S::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Infallible;
    type Future = NextResponse<S, Err>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = ready!(self.inner.poll_ready(cx));
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: WebRequest<Err>) -> Self::Future {
        let wreq = req.weak_request();
        NextResponse {
            fut: self.inner.call(req),
            req: wreq,
            _t: PhantomData,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct NextResponse<S: Service<WebRequest<Err>>, Err> {
        #[pin]
        fut: S::Future,
        req: WeakHttpRequest,
        _t: PhantomData<Err>,
    }
}

impl<S, Err> Future for NextResponse<S, Err>
where
    S: Service<WebRequest<Err>, Response = WebResponse>,
    S::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Infallible>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match ready!(this.fut.poll(cx)) {
            Ok(res) => Poll::Ready(Ok(res)),
            Err(err) => {
                let req = HttpRequest(this.req.0.upgrade().unwrap());
                Poll::Ready(Ok(WebResponse::new(err.into().error_response(&req), req)))
            }
        }
    }
}

pub struct Filter<Err>(PhantomData<Err>);

impl<Err: ErrorRenderer> Filter<Err> {
    pub(super) fn new() -> Self {
        Filter(PhantomData)
    }
}

impl<Err: ErrorRenderer> ServiceFactory<WebRequest<Err>> for Filter<Err> {
    type Response = WebRequest<Err>;
    type Error = Err::Container;
    type InitError = ();
    type Service = Filter<Err>;
    type Future = Ready<Filter<Err>, ()>;

    #[inline]
    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(Filter(PhantomData))
    }
}

impl<Err: ErrorRenderer> Service<WebRequest<Err>> for Filter<Err> {
    type Response = WebRequest<Err>;
    type Error = Err::Container;
    type Future = Ready<WebRequest<Err>, Err::Container>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: WebRequest<Err>) -> Self::Future {
        Ready::Ok(req)
    }
}
