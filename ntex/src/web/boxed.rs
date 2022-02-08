use std::{
    future::Future, marker::PhantomData, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use super::{ErrorRenderer, WebRequest, WebResponse};
use crate::service::{Service, ServiceFactory};

pub type BoxFuture<'a, Err: ErrorRenderer> =
    Pin<Box<dyn Future<Output = Result<WebResponse, Err::Container>> + 'a>>;

pub type BoxFactoryFuture<'a, Err: ErrorRenderer> =
    Pin<Box<dyn Future<Output = Result<BoxService<'a, Err>, ()>>>>;

pub type BoxService<'a, Err: ErrorRenderer> = Box<
    dyn Service<
            &'a mut WebRequest<'a, Err>,
            Response = WebResponse,
            Error = Err::Container,
            Future = BoxFuture<'a, Err>,
        > + 'static,
>;

pub struct BoxServiceFactory<'a, Err: ErrorRenderer>(Inner<'a, Err>);

/// Create boxed service factory
pub fn factory<'a, T, Err>(factory: T) -> BoxServiceFactory<'a, Err>
where
    Err: ErrorRenderer,
    T: ServiceFactory<
            &'a mut WebRequest<'a, Err>,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    T::Future: 'static,
    T::Service: 'static,
    <T::Service as Service<&'a mut WebRequest<'a, Err>>>::Future: 'a,
{
    BoxServiceFactory(FactoryWrapper::boxed(factory))
}

type Inner<'a, Err: ErrorRenderer + 'static> = Box<
    dyn ServiceFactory<
            &'a mut WebRequest<'a, Err>,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
            Service = BoxService<'a, Err>,
            Future = BoxFactoryFuture<'a, Err>,
        > + 'static,
>;

impl<'a, Err: ErrorRenderer> ServiceFactory<&'a mut WebRequest<'a, Err>>
    for BoxServiceFactory<'a, Err>
{
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = BoxService<'a, Err>;
    type Future = BoxFactoryFuture<'a, Err>;

    fn new_service(&self, cfg: ()) -> Self::Future {
        self.0.new_service(cfg)
    }
}

struct FactoryWrapper<T, Err> {
    factory: T,
    _t: PhantomData<Err>,
}

impl<'a, T, Err> FactoryWrapper<T, Err>
where
    Err: ErrorRenderer + 'static,
    T: ServiceFactory<
            &'a mut WebRequest<'a, Err>,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    T::Future: 'static,
    T::Service: 'static,
    <T::Service as Service<&'a mut WebRequest<'a, Err>>>::Future: 'a,
{
    fn boxed(factory: T) -> Inner<'a, Err> {
        Box::new(Self {
            factory,
            _t: PhantomData,
        })
    }
}

impl<'a, T, Err> ServiceFactory<&'a mut WebRequest<'a, Err>> for FactoryWrapper<T, Err>
where
    Err: ErrorRenderer + 'static,
    T: ServiceFactory<
            &'a mut WebRequest<'a, Err>,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    T::Future: 'static,
    T::Service: 'static,
    <T::Service as Service<&'a mut WebRequest<'a, Err>>>::Future: 'a,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = BoxService<'a, Err>;
    type Future = BoxFactoryFuture<'a, Err>;

    fn new_service(&self, cfg: ()) -> Self::Future {
        let fut = self.factory.new_service(cfg);
        Box::pin(async move {
            let srv = fut.await?;
            Ok(ServiceWrapper::boxed(srv))
        })
    }
}

struct ServiceWrapper<T, Err>(T, PhantomData<Err>);

impl<'a, T, Err> ServiceWrapper<T, Err>
where
    Err: ErrorRenderer,
    T: Service<&'a mut WebRequest<'a, Err>, Response = WebResponse, Error = Err::Container>
        + 'static,
    T::Future: 'a,
{
    fn boxed(service: T) -> BoxService<'a, Err> {
        Box::new(ServiceWrapper(service, PhantomData))
    }
}

impl<'a, T, Err> Service<&'a mut WebRequest<'a, Err>> for ServiceWrapper<T, Err>
where
    Err: ErrorRenderer,
    T: Service<&'a mut WebRequest<'a, Err>, Response = WebResponse, Error = Err::Container>
        + 'static,
    T::Future: 'a,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = BoxFuture<'a, Err>;

    #[inline]
    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(ctx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.0.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: &'a mut WebRequest<'a, Err>) -> Self::Future {
        Box::pin(self.0.call(req))
    }
}
