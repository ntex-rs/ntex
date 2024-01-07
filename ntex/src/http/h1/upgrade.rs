use std::{io, marker::PhantomData};

use crate::http::{h1::Codec, request::Request};
use crate::io::Io;
use crate::service::{Service, ServiceCtx, ServiceFactory};

pub struct UpgradeHandler<F>(PhantomData<F>);

impl<F> ServiceFactory<(Request, Io<F>, Codec)> for UpgradeHandler<F> {
    type Response = ();
    type Error = io::Error;

    type Service = UpgradeHandler<F>;
    type InitError = io::Error;

    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        unimplemented!()
    }
}

impl<F> Service<(Request, Io<F>, Codec)> for UpgradeHandler<F> {
    type Response = ();
    type Error = io::Error;

    async fn call(
        &self,
        _: (Request, Io<F>, Codec),
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        unimplemented!()
    }
}
