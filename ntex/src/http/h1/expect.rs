use std::io;

use crate::http::request::Request;
use crate::service::{Service, ServiceCtx, ServiceFactory};

#[derive(Copy, Clone, Debug)]
pub struct ExpectHandler;

impl ServiceFactory<Request> for ExpectHandler {
    type Response = Request;
    type Error = io::Error;
    type Service = ExpectHandler;
    type InitError = io::Error;

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        Ok(ExpectHandler)
    }
}

impl Service<Request> for ExpectHandler {
    type Response = Request;
    type Error = io::Error;

    async fn call(
        &self,
        req: Request,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        Ok(req)
    }
}
