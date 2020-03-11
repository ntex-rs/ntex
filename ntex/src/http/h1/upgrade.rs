use std::io;
use std::marker::PhantomData;
use std::task::{Context, Poll};

use actix_codec::Framed;
use futures::future::Ready;

use crate::http::h1::Codec;
use crate::http::request::Request;
use crate::{Service, ServiceFactory};

pub struct UpgradeHandler<T>(PhantomData<T>);

impl<T> ServiceFactory for UpgradeHandler<T> {
    type Config = ();
    type Request = (Request, Framed<T, Codec>);
    type Response = ();
    type Error = io::Error;
    type Service = UpgradeHandler<T>;
    type InitError = io::Error;
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        unimplemented!()
    }
}

impl<T> Service for UpgradeHandler<T> {
    type Request = (Request, Framed<T, Codec>);
    type Response = ();
    type Error = io::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Self::Request) -> Self::Future {
        unimplemented!()
    }
}
