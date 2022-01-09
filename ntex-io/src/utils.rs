use std::{marker::PhantomData, task::Context, task::Poll};

use ntex_service::{fn_factory_with_config, into_service, Service, ServiceFactory};
use ntex_util::future::Ready;

use crate::{Filter, FilterFactory, Io, IoBoxed};

/// Service that converts any Io<F> stream to IoBoxed stream
pub fn seal<F, S, C>(
    srv: S,
) -> impl ServiceFactory<
    Io<F>,
    C,
    Response = S::Response,
    Error = S::Error,
    InitError = S::InitError,
>
where
    F: Filter,
    S: ServiceFactory<IoBoxed, C>,
{
    fn_factory_with_config(move |cfg: C| {
        let fut = srv.new_service(cfg);
        async move {
            let srv = fut.await?;
            Ok(into_service(move |io: Io<F>| srv.call(IoBoxed::from(io))))
        }
    })
}

/// Create filter factory service
pub fn filter<T, F>(filter: T) -> FilterServiceFactory<T, F>
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

impl<T, F> ServiceFactory<Io<F>, ()> for FilterServiceFactory<T, F>
where
    T: FilterFactory<F> + Clone,
    F: Filter,
{
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

impl<T, F> Service<Io<F>> for FilterService<T, F>
where
    T: FilterFactory<F> + Clone,
    F: Filter,
{
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
