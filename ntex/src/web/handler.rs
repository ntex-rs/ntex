use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::{ok, Ready};
use futures::ready;
use pin_project::pin_project;

use crate::{Service, ServiceFactory};

use super::error::WebError;
use super::extract::FromRequest;
use super::request::HttpRequest;
use super::responder::Responder;
use super::service::{WebRequest, WebResponse};

/// Async handler converter factory
pub trait Factory<T, R, O, Err>: Clone + 'static
where
    R: Future<Output = O>,
    O: Responder<Err>,
{
    fn call(&self, param: T) -> R;
}

impl<F, R, O, Err> Factory<(), R, O, Err> for F
where
    F: Fn() -> R + Clone + 'static,
    R: Future<Output = O>,
    O: Responder<Err>,
{
    fn call(&self, _: ()) -> R {
        (self)()
    }
}

#[doc(hidden)]
pub struct Handler<F, T, R, O, Err>
where
    F: Factory<T, R, O, Err>,
    R: Future<Output = O>,
    O: Responder<Err>,
{
    hnd: F,
    _t: PhantomData<(T, R, O, Err)>,
}

impl<F, T, R, O, Err> Handler<F, T, R, O, Err>
where
    F: Factory<T, R, O, Err>,
    R: Future<Output = O>,
    O: Responder<Err>,
{
    pub fn new(hnd: F) -> Self {
        Handler {
            hnd,
            _t: PhantomData,
        }
    }
}

impl<F, T, R, O, Err> Clone for Handler<F, T, R, O, Err>
where
    F: Factory<T, R, O, Err>,
    R: Future<Output = O>,
    O: Responder<Err>,
{
    fn clone(&self) -> Self {
        Handler {
            hnd: self.hnd.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, T, R, O, Err> Service for Handler<F, T, R, O, Err>
where
    F: Factory<T, R, O, Err>,
    R: Future<Output = O>,
    O: Responder<Err>,
    Err: 'static,
{
    type Request = (T, HttpRequest);
    type Response = WebResponse;
    type Error = (WebError<Err>, HttpRequest);
    type Future = HandlerWebResponse<R, O, Err>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (param, req): (T, HttpRequest)) -> Self::Future {
        HandlerWebResponse {
            fut: self.hnd.call(param),
            fut2: None,
            req: Some(req),
        }
    }
}

#[doc(hidden)]
#[pin_project]
pub struct HandlerWebResponse<T, R, Err>
where
    T: Future<Output = R>,
    R: Responder<Err>,
{
    #[pin]
    fut: T,
    #[pin]
    fut2: Option<R::Future>,
    req: Option<HttpRequest>,
}

impl<T, R, Err> Future for HandlerWebResponse<T, R, Err>
where
    T: Future<Output = R>,
    R: Responder<Err>,
    Err: 'static,
{
    type Output = Result<WebResponse, (WebError<Err>, HttpRequest)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        if let Some(fut) = this.fut2.as_pin_mut() {
            return match fut.poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(res)) => {
                    Poll::Ready(Ok(WebResponse::new(this.req.take().unwrap(), res)))
                }
                Poll::Ready(Err(e)) => {
                    Poll::Ready(Ok(WebResponse::from_err(e, this.req.take().unwrap())))
                }
            };
        }

        match this.fut.poll(cx) {
            Poll::Ready(res) => {
                let fut = res.respond_to(this.req.as_ref().unwrap());
                self.as_mut().project().fut2.set(Some(fut));
                self.poll(cx)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Extract arguments from request
pub struct Extract<T: FromRequest<E>, S, E> {
    service: S,
    _t: PhantomData<(T, E)>,
}

impl<T: FromRequest<E>, S, E> Extract<T, S, E> {
    pub fn new(service: S) -> Self {
        Extract {
            service,
            _t: PhantomData,
        }
    }
}

impl<T: FromRequest<E>, S, E> ServiceFactory for Extract<T, S, E>
where
    S: Service<
            Request = (T, HttpRequest),
            Response = WebResponse,
            Error = (WebError<E>, HttpRequest),
        > + Clone,
    E: 'static,
{
    type Config = ();
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = (WebError<E>, HttpRequest);
    type InitError = ();
    type Service = ExtractService<T, S, E>;
    type Future = Ready<Result<Self::Service, ()>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ok(ExtractService {
            _t: PhantomData,
            service: self.service.clone(),
        })
    }
}

pub struct ExtractService<T: FromRequest<E>, S, E> {
    service: S,
    _t: PhantomData<(T, E)>,
}

impl<T: FromRequest<E>, S, E> Service for ExtractService<T, S, E>
where
    S: Service<
            Request = (T, HttpRequest),
            Response = WebResponse,
            Error = (WebError<E>, HttpRequest),
        > + Clone,
    E: 'static,
{
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = (WebError<E>, HttpRequest);
    type Future = ExtractResponse<T, S, E>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: WebRequest) -> Self::Future {
        let (req, mut payload) = req.into_parts();
        let fut = T::from_request(&req, &mut payload);

        ExtractResponse {
            fut,
            req,
            fut_s: None,
            service: self.service.clone(),
        }
    }
}

#[pin_project]
pub struct ExtractResponse<T: FromRequest<E>, S: Service, E> {
    req: HttpRequest,
    service: S,
    #[pin]
    fut: T::Future,
    #[pin]
    fut_s: Option<S::Future>,
}

impl<T: FromRequest<E>, S, E> Future for ExtractResponse<T, S, E>
where
    S: Service<
        Request = (T, HttpRequest),
        Response = WebResponse,
        Error = (WebError<E>, HttpRequest),
    >,
    E: 'static,
{
    type Output = Result<WebResponse, (WebError<E>, HttpRequest)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        if let Some(fut) = this.fut_s.as_pin_mut() {
            return fut.poll(cx);
        }

        match ready!(this.fut.poll(cx)) {
            Ok(item) => {
                let fut = Some(this.service.call((item, this.req.clone())));
                self.as_mut().project().fut_s.set(fut);
                self.poll(cx)
            }
            Err(e) => Poll::Ready(Ok(WebResponse::from_err(e, this.req.clone()))),
        }
    }
}

/// FromRequest trait impl for tuples
macro_rules! factory_tuple ({ $(($n:tt, $T:ident)),+} => {
    impl<Func, $($T,)+ Res, O, Err> Factory<($($T,)+), Res, O, Err> for Func
    where Func: Fn($($T,)+) -> Res + Clone + 'static,
          Res: Future<Output = O>,
          O: Responder<Err>,
          Err: 'static,
    {
        fn call(&self, param: ($($T,)+)) -> Res {
            (self)($(param.$n,)+)
        }
    }
});

#[rustfmt::skip]
mod m {
    use super::*;

factory_tuple!((0, A));
factory_tuple!((0, A), (1, B));
factory_tuple!((0, A), (1, B), (2, C));
factory_tuple!((0, A), (1, B), (2, C), (3, D));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H), (8, I));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H), (8, I), (9, J));
}
