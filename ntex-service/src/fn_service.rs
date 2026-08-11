use std::{fmt, future::Future, future::ready, marker::PhantomData};

use crate::{IntoService, IntoServiceFactory, Service, ServiceCtx, ServiceFactory};

#[inline]
/// Create `ServiceFactory` for function
pub fn fn_service<F, Req, Res, Err, Cfg>(f: F) -> FnServiceFactory<F, Req, Res, Err, Cfg>
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
///     let srv = factory.pipeline(()).await?;
///
///     // now we can use `div` service
///     let result = srv.call((10, 20), &()).await?;
///
///     println!("10 / 20 = {}", result);
///
///     Ok(())
/// }
/// ```
pub fn fn_factory<F, Srv, Err>(f: F) -> FnServiceNoConfig<F, Srv, Err>
where
    F: AsyncFn() -> Result<Srv, Err>,
    Srv: Service,
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
pub fn fn_factory_with_config<F, Cfg, Srv, Err>(f: F) -> FnServiceConfig<F, Cfg, Srv, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err>,
    Srv: Service,
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

impl<F, Req, Res, Err> Service for FnService<F, Req>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    type St = ();
    type Req = Req;
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn call(&self, req: Req, _: ServiceCtx<'_, Self>) -> Result<Res, Err> {
        (self.f)(req).await
    }
}

impl<F, Req, Res, Err> IntoService<FnService<F, Req>> for F
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

impl<F, Req, Res, Err> Service for FnServiceFactory<F, Req, Res, Err, ()>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    type St = ();
    type Req = Req;
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn call(&self, req: Req, _: ServiceCtx<'_, Self>) -> Result<Res, Err> {
        (self.f)(req).await
    }
}

impl<F, Req, Res, Err, Cfg> ServiceFactory<Req> for FnServiceFactory<F, Req, Res, Err, Cfg>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    type Res = Res;
    type Error = Err;

    type Service = FnService<F, Req>;
    type InitCfg = Cfg;
    type InitError = ();

    #[inline]
    fn create(
        &self,
        _: Cfg,
    ) -> impl Future<Output = Result<Self::Service, Self::InitError>> {
        ready(Ok(FnService {
            f: self.f.clone(),
            _t: PhantomData,
        }))
    }
}

/// `ServiceFactory` for a `AsyncFn(Cfg) -> Result<Srv, Err>` function
pub struct FnServiceConfig<F, Cfg, Srv, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err>,
    Srv: Service,
{
    f: F,
    _t: PhantomData<(Cfg, Srv, Err)>,
}

impl<F, Cfg, Srv, Err> Clone for FnServiceConfig<F, Cfg, Srv, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err> + Clone,
    Srv: Service,
{
    #[inline]
    fn clone(&self) -> Self {
        FnServiceConfig {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, Cfg, Srv, Err> fmt::Debug for FnServiceConfig<F, Cfg, Srv, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err>,
    Srv: Service,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceConfig")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Cfg, Srv, St, Req, Err> ServiceFactory<Req, St>
    for FnServiceConfig<F, Cfg, Srv, Err>
where
    F: AsyncFn(Cfg) -> Result<Srv, Err>,
    Srv: Service<St = St, Req = Req>,
{
    type Res = Srv::Res;
    type Error = Srv::Error;

    type Service = Srv;
    type InitCfg = Cfg;
    type InitError = Err;

    #[inline]
    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError> {
        (self.f)(cfg).await
    }
}

/// `ServiceFactory` for a `Fn() -> Future<Service>` function
pub struct FnServiceNoConfig<F, S, E, C = ()>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service,
{
    f: F,
    _t: PhantomData<C>,
}

impl<F, S, E, C> FnServiceNoConfig<F, S, E, C>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service,
{
    fn new(f: F) -> Self {
        Self { f, _t: PhantomData }
    }
}

impl<F, S, St, Req, E, C> ServiceFactory<Req, St> for FnServiceNoConfig<F, S, E, C>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service<St = St, Req = Req>,
    C: 'static,
{
    type Res = S::Res;
    type Error = S::Error;
    type Service = S;
    type InitCfg = C;
    type InitError = E;

    #[inline]
    async fn create(&self, _: C) -> Result<S, E> {
        (self.f)().await
    }
}

impl<F, S, E> Clone for FnServiceNoConfig<F, S, E>
where
    F: AsyncFn() -> Result<S, E> + Clone,
    S: Service,
{
    #[inline]
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

impl<F, S, E> fmt::Debug for FnServiceNoConfig<F, S, E>
where
    F: AsyncFn() -> Result<S, E>,
    S: Service,
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
    use crate::Pipeline;

    #[ntex::test]
    async fn test_fn_service() {
        let new_srv = fn_service(async |()| Ok::<_, ()>("srv")).clone();
        let _ = format!("{new_srv:?}");

        let srv = Pipeline::new(new_srv.create(()).await.unwrap()).bind(());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let srv2 = Pipeline::new(new_srv.clone()).bind(());
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

        let srv = Pipeline::new(new_srv.create(()).await.unwrap()).bind(());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let srv2 = Pipeline::new(new_srv.clone()).bind(());
        let res = srv2.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv2:?}");

        assert_eq!(lazy(|cx| srv2.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_service() {
        let srv = Pipeline::new(
            fn_service(|()| async { Ok::<_, ()>("srv") })
                .clone()
                .create(&())
                .await
                .unwrap()
                .clone(),
        )
        .bind(());

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

        let srv = Pipeline::new(new_srv.create(&1).await.unwrap()).bind(());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", 1));
    }
}
