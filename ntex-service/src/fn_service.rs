use std::{fmt, marker::PhantomData};

use crate::{Ctx, IntoService, IntoServiceFactory, Service, ServiceFactory};

#[inline]
/// Create `ServiceFactory` for function
pub fn fn_service<St, F, Req, Res, Err, Cfg>(
    f: F,
) -> FnServiceFactory<St, F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
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
///         Err(io::Error::other("divide by zdro"))
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
///     let srv = factory.pipeline(&()).await?;
///
///     // now we can use `div` service
///     let result = srv.call((10, 20), &()).await?;
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
///     let srv = factory.pipeline(&10).await?;
///
///     let result = srv.call(10, &()).await?;
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

pub struct FnService<St, F, Req> {
    f: F,
    _t: PhantomData<(St, Req)>,
}

impl<St, F, Req> Clone for FnService<St, F, Req>
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

impl<St, F, Req> fmt::Debug for FnService<St, F, Req> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnService")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<St, F, Req, Res, Err> Service for FnService<St, F, Req>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    type St = St;
    type Req = Req;
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self>) -> Result<Res, Err> {
        (self.f)(req).await
    }
}

impl<St, F, Req, Res, Err> IntoService<FnService<St, F, Req>> for F
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    #[inline]
    fn into_service(self) -> FnService<St, F, Req> {
        FnService {
            f: self,
            _t: PhantomData,
        }
    }
}

pub struct FnServiceFactory<St, F, Req, Res, Err, Cfg = ()>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    f: F,
    _t: PhantomData<(St, Req, Cfg)>,
}

impl<St, F, Req, Res, Err, Cfg> FnServiceFactory<St, F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    fn new(f: F) -> Self {
        FnServiceFactory { f, _t: PhantomData }
    }
}

impl<St, F, Req, Res, Err, Cfg> Clone for FnServiceFactory<St, F, Req, Res, Err, Cfg>
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

impl<St, F, Req, Res, Err, Cfg> fmt::Debug for FnServiceFactory<St, F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceFactory")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<St, F, Req, Res, Err> Service for FnServiceFactory<St, F, Req, Res, Err, ()>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    type St = St;
    type Req = Req;
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self>) -> Result<Res, Err> {
        (self.f)(req).await
    }
}

impl<St, F, Req, Res, Err, Cfg> ServiceFactory<Req>
    for FnServiceFactory<St, F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    type St = St;
    type Res = Res;
    type Error = Err;

    type Service = FnService<St, F, Req>;
    type InitCfg = Cfg;
    type InitError = ();

    #[inline]
    async fn create(&self, _: &Cfg) -> Result<Self::Service, Self::InitError> {
        Ok(FnService {
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

impl<St, F, Req, Res, Err, Cfg>
    IntoServiceFactory<FnServiceFactory<St, F, Req, Res, Err, Cfg>, Req> for F
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn into_factory(self) -> FnServiceFactory<St, F, Req, Res, Err, Cfg> {
        FnServiceFactory::new(self)
    }
}

// impl<St, F, Req, Res, Err, Cfg> IntoService<FnService<St, F, Req>, St, Req>
//     for FnServiceFactory<St, F, Req, Res, Err, Cfg>
// where
//     F: AsyncFn(Req) -> Result<Res, Err>,
// {
//     fn into_service(self) -> FnService<St, F, Req> {
//         FnService {
//             f: self.f,
//             _t: PhantomData,
//         }
//     }
// }

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

impl<St, F, Cfg, S, Req, Err> ServiceFactory<Req>
    for FnServiceConfig<St, F, Cfg, S, Req, Err>
where
    F: AsyncFn(&Cfg) -> Result<S, Err>,
    S: Service<St = St, Req = Req>,
{
    type St = S::St;
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

impl<St, F, S, Req, E, C> ServiceFactory<Req> for FnServiceNoConfig<St, F, S, E, C>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service<St = St, Req = Req>,
    C: 'static,
{
    type St = St;
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
    use crate::{Pipeline, ustate_chain, ustate_chain_factory};

    #[ntex::test]
    async fn test_fn_service() {
        let new_srv =
            ustate_chain_factory(fn_service(async |()| Ok::<_, ()>("srv")).clone());
        let _ = format!("{new_srv:?}");

        let srv = Pipeline::new(new_srv.create(&()).await.unwrap()).bind();
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let new_srv = ustate_chain(fn_service(async |()| Ok::<_, ()>("srv")));
        let srv2 = Pipeline::new(new_srv.clone()).bind();
        let res = srv2.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv2:?}");

        assert_eq!(lazy(|cx| srv2.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_comp() {
        let new_srv =
            ustate_chain_factory(fn_service(async |()| Ok::<_, ()>("srv"))).clone();
        let _ = format!("{new_srv:?}");

        let srv = Pipeline::new(new_srv.create(&()).await.unwrap()).bind();
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let new_srv = ustate_chain(fn_service(async |()| Ok::<_, ()>("srv"))).clone();
        let srv2 = Pipeline::new(new_srv.clone()).bind();
        let res = srv2.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv2:?}");

        assert_eq!(lazy(|cx| srv2.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_service() {
        let srv = Pipeline::new(
            ustate_chain_factory(fn_service(async |()| Ok::<_, ()>("srv")))
                .clone()
                .create(&())
                .await
                .unwrap()
                .clone(),
        )
        .bind();

        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_with_config() {
        let new_srv = fn_factory_with_config(async move |cfg: &usize| {
            let cfg = *cfg;
            Ok::<_, ()>(ustate_chain(fn_service(async move |()| {
                Ok::<_, ()>(("srv", cfg))
            })))
        })
        .clone();

        let srv = Pipeline::new(new_srv.create(&1).await.unwrap()).bind();
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", 1));
    }
}
