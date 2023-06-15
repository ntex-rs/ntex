use std::{io, marker::PhantomData};

use crate::http::{h1::Codec, request::Request};
use crate::{io::Io, service::Ctx, service::Service, service::ServiceFactory, util::Ready};

pub struct UpgradeHandler<F>(PhantomData<F>);

impl<F> ServiceFactory<(Request, Io<F>, Codec)> for UpgradeHandler<F> {
    type Response = ();
    type Error = io::Error;

    type Service = UpgradeHandler<F>;
    type InitError = io::Error;
    type Future<'f> = Ready<Self::Service, Self::InitError> where Self: 'f;

    #[inline]
    fn create(&self, _: ()) -> Self::Future<'_> {
        unimplemented!()
    }
}

impl<F> Service<(Request, Io<F>, Codec)> for UpgradeHandler<F> {
    type Response = ();
    type Error = io::Error;
    type Future<'f> = Ready<Self::Response, Self::Error> where F: 'f;

    #[inline]
    fn call<'a>(
        &'a self,
        _: (Request, Io<F>, Codec),
        _: Ctx<'a, Self>,
    ) -> Self::Future<'a> {
        unimplemented!()
    }
}
