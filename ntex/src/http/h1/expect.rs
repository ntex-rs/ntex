use std::io;

use crate::service::{Ctx, Service, ServiceFactory};
use crate::{http::request::Request, util::Ready};

pub struct ExpectHandler;

impl ServiceFactory<Request> for ExpectHandler {
    type Response = Request;
    type Error = io::Error;
    type Service = ExpectHandler;
    type InitError = io::Error;
    type Future<'f> = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn create(&self, _: ()) -> Self::Future<'_> {
        Ready::Ok(ExpectHandler)
    }
}

impl Service<Request> for ExpectHandler {
    type Response = Request;
    type Error = io::Error;
    type Future<'f> = Ready<Self::Response, Self::Error>;

    #[inline]
    fn call<'a>(&'a self, req: Request, _: Ctx<'a, Self>) -> Self::Future<'_> {
        Ready::Ok(req)
    }
}
