use std::{io, marker::PhantomData, task::Context, task::Poll};

use crate::http::h1::Codec;
use crate::http::request::Request;
use crate::{framed::State, util::Ready, Service, ServiceFactory};

pub struct UpgradeHandler<T>(PhantomData<T>);

impl<T> ServiceFactory for UpgradeHandler<T> {
    type Config = ();
    type Request = (Request, T, State, Codec);
    type Response = ();
    type Error = io::Error;
    type Service = UpgradeHandler<T>;
    type InitError = io::Error;
    type Future = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn new_service(&self, _: ()) -> Self::Future {
        unimplemented!()
    }
}

impl<T> Service for UpgradeHandler<T> {
    type Request = (Request, T, State, Codec);
    type Response = ();
    type Error = io::Error;
    type Future = Ready<Self::Response, Self::Error>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, _: Self::Request) -> Self::Future {
        unimplemented!()
    }
}
