use std::io;
use std::marker::PhantomData;
use std::task::{Context, Poll};

use futures::future::Ready;

use crate::codec::Framed;
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

    #[inline]
    fn new_service(&self, _: ()) -> Self::Future {
        unimplemented!()
    }
}

impl<T> Service for UpgradeHandler<T> {
    type Request = (Request, Framed<T, Codec>);
    type Response = ();
    type Error = io::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, _: Self::Request) -> Self::Future {
        unimplemented!()
    }
}
