use std::io;

use crate::http::request::Request;
use crate::{service::Service, service::ServiceFactory, util::Ready};

pub struct ExpectHandler;

impl ServiceFactory<Request> for ExpectHandler {
    type Response = Request;
    type Error = io::Error;
    type Service = ExpectHandler;
    type InitError = io::Error;
    type Future = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn create(&self, _: ()) -> Self::Future {
        Ready::Ok(ExpectHandler)
    }
}

impl Service<Request> for ExpectHandler {
    type Response = Request;
    type Error = io::Error;
    type Future<'f> = Ready<Self::Response, Self::Error>;

    #[inline]
    fn call(&self, req: Request) -> Self::Future<'_> {
        Ready::Ok(req)
    }
}
