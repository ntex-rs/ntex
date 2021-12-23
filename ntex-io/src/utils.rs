use std::{marker::PhantomData, task::Context, task::Poll};

use ntex_service::{fn_factory_with_config, into_service, Service, ServiceFactory};
use ntex_util::future::Ready;

use super::{Filter, FilterFactory, Io, IoBoxed};

/// Service that converts any Io<F> stream to IoBoxed stream
pub fn seal<F, S>(
    srv: S,
) -> impl ServiceFactory<
    Config = S::Config,
    Request = Io<F>,
    Response = S::Response,
    Error = S::Error,
    InitError = S::InitError,
>
where
    F: Filter + 'static,
    S: ServiceFactory<Request = IoBoxed>,
{
    fn_factory_with_config(move |cfg: S::Config| {
        let fut = srv.new_service(cfg);
        async move {
            let srv = fut.await?;
            Ok(into_service(move |io: Io<F>| srv.call(io.seal())))
        }
    })
}

/// Create filter factory service
pub fn filter_factory<T, F>(filter: T) -> FilterServiceFactory<T, F>
where
    T: FilterFactory<F> + Clone,
    F: Filter,
{
    FilterServiceFactory {
        filter,
        _t: PhantomData,
    }
}

pub struct FilterServiceFactory<T, F> {
    filter: T,
    _t: PhantomData<F>,
}

impl<T, F> ServiceFactory for FilterServiceFactory<T, F>
where
    T: FilterFactory<F> + Clone,
    F: Filter,
{
    type Config = ();
    type Request = Io<F>;
    type Response = Io<T::Filter>;
    type Error = T::Error;
    type Service = FilterService<T, F>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(FilterService {
            filter: self.filter.clone(),
            _t: PhantomData,
        })
    }
}

pub struct FilterService<T, F> {
    filter: T,
    _t: PhantomData<F>,
}

impl<T, F> Service for FilterService<T, F>
where
    T: FilterFactory<F> + Clone,
    F: Filter,
{
    type Request = Io<F>;
    type Response = Io<T::Filter>;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, req: Io<F>) -> Self::Future {
        req.add_filter(self.filter.clone())
    }
}
