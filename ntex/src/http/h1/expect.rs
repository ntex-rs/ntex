use std::io;
use std::task::{Context, Poll};

use futures::future::{ok, Ready};

use crate::http::request::Request;
use crate::{Service, ServiceFactory};

pub struct ExpectHandler;

impl ServiceFactory for ExpectHandler {
    type Config = ();
    type Request = Request;
    type Response = Request;
    type Error = io::Error;
    type Service = ExpectHandler;
    type InitError = io::Error;
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    #[inline]
    fn new_service(&self, _: ()) -> Self::Future {
        ok(ExpectHandler)
    }
}

impl Service for ExpectHandler {
    type Request = Request;
    type Response = Request;
    type Error = io::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: Request) -> Self::Future {
        ok(req)
    }
}
