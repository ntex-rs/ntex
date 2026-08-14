use std::{fmt, marker::PhantomData};

use crate::{IntoService, IntoServiceFactory, Service, ServiceCtx};

#[inline]
/// Create `ServiceFactory` for function that can act as a `Service`
pub fn fn_service<F, Req, Res, Err>(f: F) -> FnService<F, Req>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    FnService { f, _t: PhantomData }
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
///     let srv = factory.pipeline(&(), &()).await?;
///
///     // now we can use `div` service
///     let result = srv.call((10, 20)).await?;
///
///     println!("10 / 20 = {}", result);
///
///     Ok(())
/// }
/// ```
pub fn fn_factory<F, Srv, Req, Err>(f: F) -> FnServiceNoConfig<F, Srv, Req, Err>
where
    F: AsyncFn() -> Result<Srv, Err>,
    Srv: Service<Req>,
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
///     let srv = factory.pipeline(&10, &()).await?;
///
///     let result = srv.call(10).await?;
///     assert_eq!(result, 100);
///
///     println!("10 * 10 = {}", result);
///     Ok(())
/// }
/// ```
pub fn fn_factory_with_config<F, Cfg, Srv, Req, Err>(
    f: F,
) -> FnServiceConfig<F, Cfg, Srv, Req, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err>,
    Srv: Service<Req>,
{
    FnServiceConfig { f, _t: PhantomData }
}

pub struct FnService<F, Req> {
    f: F,
    _t: PhantomData<Req>,
}

impl<F, Req> Clone for FnService<F, Req>
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

impl<F, Req> fmt::Debug for FnService<F, Req> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnService")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Req, Res, Err> Service<Req> for FnService<F, Req>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    type Response = Res;
    type Error = Err;
    type Data = ();

    #[inline]
    async fn call(
        &self,
        req: Req,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Res, Err> {
        (self.f)(req).await
    }
}

impl<F, Req, Res, Err> IntoService<FnService<F, Req>, Req> for F
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    #[inline]
    fn into_service(self) -> FnService<F, Req> {
        FnService {
            f: self,
            _t: PhantomData,
        }
    }
}

pub struct FnServiceFactory<F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
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
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceFactory")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Req, Res, Err, Cfg> Service<Cfg> for FnServiceFactory<F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    type Response = FnService<F, Req>;
    type Error = ();
    type Data = ();

    #[inline]
    async fn call(
        &self,
        _: Cfg,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        Ok(FnService {
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

impl<F, Req, Res, Err, Cfg>
    IntoServiceFactory<FnServiceFactory<F, Req, Res, Err, Cfg>, Req, Cfg> for F
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn into_factory(self) -> FnServiceFactory<F, Req, Res, Err, Cfg> {
        FnServiceFactory::new(self)
    }
}

impl<F, Req, Res, Err, Cfg>
    IntoServiceFactory<FnServiceFactory<F, Req, Res, Err, Cfg>, Req, Cfg>
    for FnService<F, Req>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn into_factory(self) -> FnServiceFactory<F, Req, Res, Err, Cfg> {
        FnServiceFactory::new(self.f)
    }
}

/// `ServiceFactory` for a `AsyncFn(Cfg) -> Result<Srv, Err>` function
pub struct FnServiceConfig<F, Cfg, Srv, Req, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err>,
    Srv: Service<Req>,
{
    f: F,
    _t: PhantomData<(Cfg, Srv, Req, Err)>,
}

impl<F, Cfg, Srv, Req, Err> Clone for FnServiceConfig<F, Cfg, Srv, Req, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err> + Clone,
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

impl<F, Cfg, Srv, Req, Err> fmt::Debug for FnServiceConfig<F, Cfg, Srv, Req, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err>,
    Srv: Service<Req>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceConfig")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Cfg, Srv, Req, Err> Service<Cfg> for FnServiceConfig<F, Cfg, Srv, Req, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err>,
    Srv: Service<Req>,
{
    type Response = Srv;
    type Error = Err;
    type Data = Srv::Data;

    #[inline]
    async fn call(
        &self,
        cfg: Cfg,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        (self.f)(cfg).await
    }
}

/// `ServiceFactory` for a `Fn() -> Future<Service>` function
pub struct FnServiceNoConfig<F, S, Req, E>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service<Req>,
{
    f: F,
    _t: PhantomData<Req>,
}

impl<F, S, Req, E> FnServiceNoConfig<F, S, Req, E>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service<Req>,
{
    fn new(f: F) -> Self {
        Self { f, _t: PhantomData }
    }
}

impl<F, S, Req, E, C> Service<C> for FnServiceNoConfig<F, S, Req, E>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service<Req>,
    C: 'static,
{
    type Response = S;
    type Error = E;
    type Data = S::Data;

    #[inline]
    async fn call(&self, _: C, _: &Self::Data, _: ServiceCtx<'_, Self>) -> Result<S, E> {
        (self.f)().await
    }
}

impl<F, S, Req, E> Clone for FnServiceNoConfig<F, S, Req, E>
where
    F: AsyncFn() -> Result<S, E> + Clone,
    S: Service<Req>,
{
    #[inline]
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

impl<F, S, Req, E> fmt::Debug for FnServiceNoConfig<F, S, Req, E>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service<Req>,
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
    use crate::{Pipeline, ServiceFactory};

    #[ntex::test]
    async fn test_fn_service() {
        let new_srv = fn_service(async |()| Ok::<_, ()>("srv")).clone();
        let _ = format!("{new_srv:?}");

        let srv = Pipeline::new(new_srv.clone(), ()).bind();
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let srv2 = Pipeline::new(new_srv.clone(), ()).bind();
        let res = srv2.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv2:?}");

        assert_eq!(lazy(|cx| srv2.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_comp() {
        let new_srv = fn_service(|()| async { Ok::<_, ()>("srv") }).clone();
        let _ = format!("{new_srv:?}");

        let srv = Pipeline::new(new_srv.clone(), ()).bind();
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let srv2 = Pipeline::new(new_srv.clone(), ()).bind();
        let res = srv2.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv2:?}");

        assert_eq!(lazy(|cx| srv2.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_service() {
        let srv =
            Pipeline::new(fn_service(|()| async { Ok::<_, ()>("srv") }).clone(), ()).bind();

        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
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

        let srv = new_srv.pipeline(&1, &()).await.unwrap().bind();
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", 1));
    }
}
