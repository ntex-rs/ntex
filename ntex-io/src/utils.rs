use ntex_service::{fn_factory_with_config, into_service, Service, ServiceFactory};

use super::{Filter, Io, IoBoxed, IoStream};

/// Service that converts any Io<F> stream to IoBoxed stream
pub fn into_boxed<F, S>(
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
            Ok(into_service(move |io: Io<F>| srv.call(io.into_boxed())))
        }
    })
}

/// Service that converts IoStream stream to IoBoxed stream
pub fn from_iostream<S, I>(
    srv: S,
) -> impl ServiceFactory<
    Config = S::Config,
    Request = I,
    Response = S::Response,
    Error = S::Error,
    InitError = S::InitError,
>
where
    I: IoStream,
    S: ServiceFactory<Request = IoBoxed>,
{
    fn_factory_with_config(move |cfg: S::Config| {
        let fut = srv.new_service(cfg);
        async move {
            let srv = fut.await?;
            Ok(into_service(move |io| srv.call(Io::new(io).into_boxed())))
        }
    })
}
