use std::task::{Context, Poll};
use std::{future::ready, future::Future, future::Ready, marker::PhantomData};

use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};

#[inline]
/// Create `ServiceFactory` for function that can act as a `Service`
pub fn fn_service<F, Fut, Req, Res, Err, Cfg>(
    f: F,
) -> FnServiceFactory<F, Fut, Req, Res, Err, Cfg>
where
    F: Fn(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    FnServiceFactory::new(f)
}

#[inline]
/// Create `ServiceFactory` for function that can produce services
///
/// # Example
///
/// ```rust
/// use std::io;
/// use ntex_service::{fn_factory, fn_service, Service, ServiceFactory};
///
/// /// Service that divides two usize values.
/// async fn div((x, y): (usize, usize)) -> Result<usize, io::Error> {
///     if y == 0 {
///         Err(io::Error::new(io::ErrorKind::Other, "divide by zdro"))
///     } else {
///         Ok(x / y)
///     }
/// }
///
/// #[ntex::main]
/// async fn main() -> io::Result<()> {
///     // Create service factory that produces `div` services
///     let factory = fn_factory(|| {
///         async {Ok::<_, io::Error>(fn_service(div))}
///     });
///
///     // construct new service
///     let srv = factory.create(&()).await?;
///
///     // now we can use `div` service
///     let result = srv.call((10, 20)).await?;
///
///     println!("10 / 20 = {}", result);
///
///     Ok(())
/// }
/// ```
pub fn fn_factory<F, Cfg, Srv, Fut, Req, Err>(
    f: F,
) -> FnServiceNoConfig<F, Cfg, Srv, Fut, Req, Err>
where
    Srv: Service<Req>,
    F: Fn() -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
{
    FnServiceNoConfig::new(f)
}

#[inline]
/// Create `ServiceFactory` for function that accepts config argument and can produce services
///
/// Any function that has following form `Fn(Config) -> Future<Output = Service>` could
/// act as a `ServiceFactory`.
///
/// # Example
///
/// ```rust
/// use std::io;
/// use ntex_service::{fn_factory_with_config, fn_service, Service, ServiceFactory};
///
/// #[ntex::main]
/// async fn main() -> io::Result<()> {
///     // Create service factory. factory uses config argument for
///     // services it generates.
///     let factory = fn_factory_with_config(|y: &usize| {
///         let y = *y;
///         async move { Ok::<_, io::Error>(fn_service(move |x: usize| async move { Ok::<_, io::Error>(x * y) })) }
///     });
///
///     // construct new service with config argument
///     let srv = factory.create(&10).await?;
///
///     let result = srv.call(10).await?;
///     assert_eq!(result, 100);
///
///     println!("10 * 10 = {}", result);
///     Ok(())
/// }
/// ```
pub fn fn_factory_with_config<F, Fut, Cfg, Srv, Req, Err>(
    f: F,
) -> FnServiceConfig<F, Fut, Cfg, Srv, Req, Err>
where
    F: Fn(&Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service<Req>,
{
    FnServiceConfig { f, _t: PhantomData }
}

pub struct FnService<F, Fut, Req, Res, Err>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    f: F,
    _t: PhantomData<Req>,
}

impl<F, Fut, Req, Res, Err> FnService<F, Fut, Req, Res, Err>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    pub(crate) fn new(f: F) -> Self {
        Self { f, _t: PhantomData }
    }
}

impl<F, Fut, Req, Res, Err> Clone for FnService<F, Fut, Req, Res, Err>
where
    F: Fn(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Req, Res, Err> Service<Req> for FnService<F, Fut, Req, Res, Err>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    type Response = Res;
    type Error = Err;
    type Future<'f> = Fut where Self: 'f;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(&self, _: &mut Context<'_>, _: bool) -> Poll<()> {
        Poll::Ready(())
    }

    #[inline]
    fn call(&self, req: Req) -> Self::Future<'_> {
        (self.f)(req)
    }
}

impl<F, Fut, Req, Res, Err> IntoService<FnService<F, Fut, Req, Res, Err>, Req> for F
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    #[inline]
    fn into_service(self) -> FnService<F, Fut, Req, Res, Err> {
        FnService::new(self)
    }
}

pub struct FnServiceFactory<F, Fut, Req, Res, Err, Cfg>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    f: F,
    _t: PhantomData<(Req, Cfg)>,
}

impl<F, Fut, Req, Res, Err, Cfg> FnServiceFactory<F, Fut, Req, Res, Err, Cfg>
where
    F: Fn(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    fn new(f: F) -> Self {
        FnServiceFactory { f, _t: PhantomData }
    }
}

impl<F, Fut, Req, Res, Err, Cfg> Clone for FnServiceFactory<F, Fut, Req, Res, Err, Cfg>
where
    F: Fn(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Req, Res, Err> Service<Req> for FnServiceFactory<F, Fut, Req, Res, Err, ()>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    type Response = Res;
    type Error = Err;
    type Future<'f> = Fut where Self: 'f;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(&self, _: &mut Context<'_>, _: bool) -> Poll<()> {
        Poll::Ready(())
    }

    #[inline]
    fn call(&self, req: Req) -> Self::Future<'_> {
        (self.f)(req)
    }
}

impl<F, Fut, Req, Res, Err, Cfg> ServiceFactory<Req, Cfg>
    for FnServiceFactory<F, Fut, Req, Res, Err, Cfg>
where
    F: Fn(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    type Response = Res;
    type Error = Err;

    type Service = FnService<F, Fut, Req, Res, Err>;
    type InitError = ();
    type Future<'f> = Ready<Result<Self::Service, Self::InitError>> where Self: 'f;

    #[inline]
    fn create<'a>(&'a self, _: &'a Cfg) -> Self::Future<'a> {
        ready(Ok(FnService {
            f: self.f.clone(),
            _t: PhantomData,
        }))
    }
}

impl<F, Fut, Req, Res, Err, Cfg>
    IntoServiceFactory<FnServiceFactory<F, Fut, Req, Res, Err, Cfg>, Req, Cfg> for F
where
    F: Fn(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    #[inline]
    fn into_factory(self) -> FnServiceFactory<F, Fut, Req, Res, Err, Cfg> {
        FnServiceFactory::new(self)
    }
}

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService
pub struct FnServiceConfig<F, Fut, Cfg, Srv, Req, Err>
where
    F: Fn(&Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service<Req>,
{
    f: F,
    _t: PhantomData<(Fut, Cfg, Srv, Req, Err)>,
}

impl<F, Fut, Cfg, Srv, Req, Err> Clone for FnServiceConfig<F, Fut, Cfg, Srv, Req, Err>
where
    F: Fn(&Cfg) -> Fut + Clone,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service<Req>,
{
    #[inline]
    fn clone(&self) -> Self {
        FnServiceConfig {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Cfg, Srv, Req, Err> ServiceFactory<Req, Cfg>
    for FnServiceConfig<F, Fut, Cfg, Srv, Req, Err>
where
    F: Fn(&Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service<Req>,
{
    type Response = Srv::Response;
    type Error = Srv::Error;

    type Service = Srv;
    type InitError = Err;
    type Future<'f> = Fut where Self: 'f, Cfg: 'f;

    #[inline]
    fn create<'a>(&'a self, cfg: &'a Cfg) -> Self::Future<'a> {
        (self.f)(cfg)
    }
}

/// Converter for `Fn() -> Future<Service>` fn
pub struct FnServiceNoConfig<F, C, S, R, Req, E>
where
    F: Fn() -> R,
    S: Service<Req>,
    R: Future<Output = Result<S, E>>,
{
    f: F,
    _t: PhantomData<(Req, C)>,
}

impl<F, C, S, R, Req, E> FnServiceNoConfig<F, C, S, R, Req, E>
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service<Req>,
{
    fn new(f: F) -> Self {
        Self { f, _t: PhantomData }
    }
}

impl<F, C, S, R, Req, E> ServiceFactory<Req, C> for FnServiceNoConfig<F, C, S, R, Req, E>
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;
    type InitError = E;
    type Future<'f> = R where Self: 'f, R: 'f, C: 'f;

    #[inline]
    fn create<'a>(&'a self, _: &'a C) -> Self::Future<'a> {
        (self.f)()
    }
}

impl<F, C, S, R, Req, E> Clone for FnServiceNoConfig<F, C, S, R, Req, E>
where
    F: Fn() -> R + Clone,
    R: Future<Output = Result<S, E>>,
    S: Service<Req>,
{
    #[inline]
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

impl<F, C, S, R, Req, E> IntoServiceFactory<FnServiceNoConfig<F, C, S, R, Req, E>, Req, C>
    for F
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service<Req>,
{
    #[inline]
    fn into_factory(self) -> FnServiceNoConfig<F, C, S, R, Req, E> {
        FnServiceNoConfig::new(self)
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::lazy;

    use super::*;
    use crate::{Service, ServiceFactory};

    #[ntex::test]
    async fn test_fn_service() {
        let new_srv = fn_service(|()| async { Ok::<_, ()>("srv") }).clone();

        let srv = new_srv.create(&()).await.unwrap();
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");

        let srv2 = new_srv.clone();
        let res = srv2.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");

        assert_eq!(
            lazy(|cx| srv2.poll_shutdown(cx, false)).await,
            Poll::Ready(())
        );
    }

    #[ntex::test]
    async fn test_fn_service_service() {
        let srv = fn_service(|()| async { Ok::<_, ()>("srv") })
            .clone()
            .create(&())
            .await
            .unwrap()
            .clone();

        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        assert_eq!(
            lazy(|cx| srv.poll_shutdown(cx, false)).await,
            Poll::Ready(())
        );
    }

    #[ntex::test]
    async fn test_fn_service_with_config() {
        let new_srv = fn_factory_with_config(|cfg: &usize| {
            let cfg = *cfg;
            async move {
                Ok::<_, ()>(fn_service(
                    move |()| async move { Ok::<_, ()>(("srv", cfg)) },
                ))
            }
        })
        .clone();

        let srv = new_srv.create(&1).await.unwrap();
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", 1));
    }
}
