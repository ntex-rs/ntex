use std::{convert::Infallible, fmt, marker::PhantomData};

use crate::{Ctx, IntoService, IntoServiceFactory, Service, ServiceFactory};

/// `Service` implementation for an `AsyncFn(Req) -> Result<Res, Err>` fn.
#[inline]
pub fn fn_service<F, Req, Res, Err>(f: F) -> FnService<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    FnService { f, _t: PhantomData }
}

/// `Service` implementation for an `AsyncFn(Req, &St) -> Result<Res, Err>` function.
///
/// This service accesses the pipeline state via the second `&St` parameter.
#[inline]
pub fn fn_service_st<F, St, Req, Res, Err>(f: F) -> FnServiceSt<F, St, Req, Res, Err>
where
    F: AsyncFn(Req, &St) -> Result<Res, Err>,
{
    FnServiceSt { f, _t: PhantomData }
}

#[inline]
/// Create `ServiceFactory` for function that can produce services
///
/// # Example
///
/// ```rust
/// use std::io;
/// use ntex_service::{factory, fn_factory, fn_service, Pipeline, Service, ServiceFactory};
///
/// /// Service that divides two usize values.
/// async fn div((x, y): (usize, usize)) -> Result<usize, io::Error> {
///     if y == 0 {
///         Err(io::Error::other("divide by zdro"))
///     } else {
///         Ok(x / y)
///     }
/// }
///
/// #[ntex::main]
/// async fn main() -> io::Result<()> {
///     // Create service factory that produces `div` services
///     let fac = fn_factory(async || {
///         Ok::<_, io::Error>(fn_service(div))
///     });
///
///     // construct new service
///     let srv = Pipeline::new(factory(fac).create(&()).await?);
///
///     // now we can use `div` service
///     let result = srv.call((10, 20)).await?;
///
///     println!("10 / 20 = {}", result);
///
///     Ok(())
/// }
/// ```
pub fn fn_factory<St, F, Srv, Err>(f: F) -> FnServiceNoConfig<St, F, Srv, Err>
where
    F: AsyncFn() -> Result<Srv, Err>,
{
    FnServiceNoConfig::new(f)
}

#[inline]
/// Create `ServiceFactory` for function that accepts config argument and can produce services
///
/// Any function that has following form `AsyncFn(Config) -> Result<Service, Error>` could
/// act as a `ServiceFactory`.
///
/// # Example
///
/// ```rust
/// use std::io;
/// use ntex_service::{factory, fn_factory_with_config, fn_service, Pipeline, Service, ServiceFactory};
///
/// #[ntex::main]
/// async fn main() -> io::Result<()> {
///     // Create service factory. factory uses config argument for
///     // services it generates.
///     let fac = factory(fn_factory_with_config(async |y: &usize| {
///         let y = *y;
///         Ok::<_, io::Error>(fn_service(move |x: usize| async move { Ok::<_, io::Error>(x * y) }))
///     }));
///
///     // construct new service with config argument
///     let srv = Pipeline::new(factory(fac).create(&10).await?);
///
///     let result = srv.call(10).await?;
///     assert_eq!(result, 100);
///
///     println!("10 * 10 = {}", result);
///     Ok(())
/// }
/// ```
pub fn fn_factory_with_config<St, F, Cfg, Srv, Req, Err>(
    f: F,
) -> FnServiceConfig<St, F, Cfg, Srv, Req, Err>
where
    F: AsyncFn(&Cfg) -> Result<Srv, Err>,
{
    FnServiceConfig { f, _t: PhantomData }
}

/// `Service` implementation for an `AsyncFn(Req) -> Result<Res, Err>` fn.
pub struct FnService<F, Req, Res, Err> {
    f: F,
    _t: PhantomData<(Req, Res, Err)>,
}

impl<F, Req, Res, Err> Clone for FnService<F, Req, Res, Err>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, Req, Res, Err> fmt::Debug for FnService<F, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnService")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, St, Req, Res, Err> Service<St> for FnService<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    type Req = Req;
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<Res, Err> {
        (self.f)(req).await
    }
}

impl<F, St, Req, Res, Err, Cfg> IntoServiceFactory<FnServiceFactory<F, Req, Res, Err, Cfg>, St, Req>
    for FnService<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn into_factory(self) -> FnServiceFactory<F, Req, Res, Err, Cfg> {
        FnServiceFactory {
            f: self.f,
            _t: PhantomData,
        }
    }
}

impl<F, St, Req, Res, Err> IntoService<FnService<F, Req, Res, Err>, St> for F
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    #[inline]
    fn into_service(self) -> FnService<F, Req, Res, Err> {
        FnService {
            f: self,
            _t: PhantomData,
        }
    }
}

/// `Service` implementation for an `AsyncFn(Req, &St) -> Result<Res, Err>` function.
///
/// This service accesses the pipeline state via the second `&St` parameter.
pub struct FnServiceSt<F, St, Req, Res, Err> {
    f: F,
    _t: PhantomData<(St, Req, Res, Err)>,
}

impl<F, St, Req, Res, Err> Clone for FnServiceSt<F, St, Req, Res, Err>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, St, Req, Res, Err> fmt::Debug for FnServiceSt<F, St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceSt")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, St, Req, Res, Err> Service<St> for FnServiceSt<F, St, Req, Res, Err>
where
    F: AsyncFn(Req, &St) -> Result<Res, Err>,
{
    type Req = Req;
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Res, Err> {
        (self.f)(req, ctx.st()).await
    }
}

impl<F, St, Req, Res, Err> IntoService<FnServiceSt<F, St, Req, Res, Err>, St> for F
where
    F: AsyncFn(Req, &St) -> Result<Res, Err>,
{
    #[inline]
    fn into_service(self) -> FnServiceSt<F, St, Req, Res, Err> {
        FnServiceSt {
            f: self,
            _t: PhantomData,
        }
    }
}

pub struct FnServiceFactory<F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    f: F,
    _t: PhantomData<(Req, Cfg)>,
}

impl<F, Req, Res, Err, Cfg> FnServiceFactory<F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    fn new(f: F) -> Self {
        FnServiceFactory { f, _t: PhantomData }
    }
}

impl<F, Req, Res, Err, Cfg> Clone for FnServiceFactory<F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, Req, Res, Err, Cfg> fmt::Debug for FnServiceFactory<F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceFactory")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, St, Req, Res, Err, Cfg> ServiceFactory<Req, St> for FnServiceFactory<F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    type Res = Res;
    type Error = Err;

    type Service = FnService<F, Req, Res, Err>;
    type InitCfg = Cfg;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &Cfg) -> Result<Self::Service, Self::InitError> {
        Ok(FnService {
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

impl<St, F, Req, Res, Err, Cfg> IntoServiceFactory<FnServiceFactory<F, Req, Res, Err, Cfg>, St, Req>
    for F
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn into_factory(self) -> FnServiceFactory<F, Req, Res, Err, Cfg> {
        FnServiceFactory::new(self)
    }
}

/// `ServiceFactory` for a `AsyncFn(Cfg) -> Result<Srv, Err>` function
pub struct FnServiceConfig<St, F, Cfg, Srv, Req, Err>
where
    F: AsyncFn(&Cfg) -> Result<Srv, Err>,
{
    f: F,
    _t: PhantomData<(St, Cfg, Srv, Req, Err)>,
}

impl<St, F, Cfg, Srv, Req, Err> Clone for FnServiceConfig<St, F, Cfg, Srv, Req, Err>
where
    F: AsyncFn(&Cfg) -> Result<Srv, Err> + Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        FnServiceConfig {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<St, F, Cfg, Srv, Req, Err> fmt::Debug for FnServiceConfig<St, F, Cfg, Srv, Req, Err>
where
    F: AsyncFn(&Cfg) -> Result<Srv, Err>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceConfig")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<St, F, Cfg, S, Req, Err> ServiceFactory<Req, St> for FnServiceConfig<St, F, Cfg, S, Req, Err>
where
    F: AsyncFn(&Cfg) -> Result<S, Err>,
    S: Service<St, Req = Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    type Service = S;
    type InitCfg = Cfg;
    type InitError = Err;

    #[inline]
    async fn create(&self, cfg: &Cfg) -> Result<Self::Service, Self::InitError> {
        (self.f)(cfg).await
    }
}

/// `ServiceFactory` for a `Fn() -> Future<Service>` function
pub struct FnServiceNoConfig<St, F, S, E, C = ()>
where
    F: AsyncFn() -> Result<S, E>,
{
    f: F,
    _t: PhantomData<(St, C)>,
}

impl<St, F, S, E, C> FnServiceNoConfig<St, F, S, E, C>
where
    F: AsyncFn() -> Result<S, E>,
{
    fn new(f: F) -> Self {
        Self { f, _t: PhantomData }
    }
}

impl<St, F, S, Req, E, C> ServiceFactory<Req, St> for FnServiceNoConfig<St, F, S, E, C>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service<St, Req = Req>,
    C: 'static,
{
    type Res = S::Res;
    type Error = S::Error;
    type Service = S;
    type InitCfg = C;
    type InitError = E;

    #[inline]
    async fn create(&self, _: &C) -> Result<S, E> {
        (self.f)().await
    }
}

impl<St, F, S, E, C> Clone for FnServiceNoConfig<St, F, S, E, C>
where
    F: AsyncFn() -> Result<S, E> + Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

impl<St, F, S, E, C> fmt::Debug for FnServiceNoConfig<St, F, S, E, C>
where
    F: AsyncFn() -> Result<S, E>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceNoConfig")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use ntex::util::lazy;
    use std::task::Poll;

    use super::*;
    use crate::{Pipeline, factory};

    #[ntex::test]
    async fn test_fn_service() {
        let new_srv = factory(fn_service(async |()| Ok::<_, ()>("srv")).clone());
        let _ = format!("{new_srv:?}");

        let srv = Pipeline::new(new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let new_srv = fn_service(async |()| Ok::<_, ()>("srv"));
        let srv = Pipeline::new(new_srv.clone());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_comp() {
        let new_srv = fn_service(async |()| Ok::<_, ()>("srv")).clone();
        let _ = format!("{new_srv:?}");

        let srv = Pipeline::new(factory(new_srv).create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let new_srv = fn_service(async |()| Ok::<_, ()>("srv")).clone();
        let srv = Pipeline::new(new_srv.clone());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_service() {
        let srv = Pipeline::new(
            factory(fn_service(async |()| Ok::<_, ()>("srv")).clone())
                .create(&())
                .await
                .unwrap()
                .clone(),
        );

        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_with_config() {
        let new_srv = factory(fn_factory_with_config(async move |cfg: &usize| {
            let cfg = *cfg;
            Ok::<_, ()>(fn_service(async move |()| Ok::<_, ()>(("srv", cfg))))
        }))
        .clone();

        let srv = Pipeline::new(new_srv.create(&1).await.unwrap());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", 1));
    }
}
