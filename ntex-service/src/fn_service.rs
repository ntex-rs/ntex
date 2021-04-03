use std::task::{Context, Poll};
use std::{cell::Cell, cell::RefCell, future::Future, marker::PhantomData};

use ntex_util::future::Ready;

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
/// Create `Service` for mut function that can act as a `Service`
pub fn fn_mut_service<F, Fut, Req, Res, Err>(f: F) -> FnMutService<F, Fut, Req, Res, Err>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    FnMutService::new(f)
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
///     let srv = factory.new_service(()).await?;
///
///     // now we can use `div` service
///     let result = srv.call((10, 20)).await?;
///
///     println!("10 / 20 = {}", result);
///
///     Ok(())
/// }
/// ```
pub fn fn_factory<F, Cfg, Srv, Fut, Err>(
    f: F,
) -> FnServiceNoConfig<F, Cfg, Srv, Fut, Err>
where
    Srv: Service,
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
///     let factory = fn_factory_with_config(|y: usize| {
///         async move { Ok::<_, io::Error>(fn_service(move |x: usize| async move { Ok::<_, io::Error>(x * y) })) }
///     });
///
///     // construct new service with config argument
///     let srv = factory.new_service(10).await?;
///
///     let result = srv.call(10).await?;
///     assert_eq!(result, 100);
///
///     println!("10 * 10 = {}", result);
///     Ok(())
/// }
/// ```
pub fn fn_factory_with_config<F, Fut, Cfg, Srv, Err>(
    f: F,
) -> FnServiceConfig<F, Fut, Cfg, Srv, Err>
where
    F: Fn(Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service,
{
    FnServiceConfig::new(f)
}

#[inline]
pub fn fn_shutdown() {}

pub struct FnService<F, Fut, Req, Res, Err, FShut = fn()>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    f: F,
    f_shutdown: Cell<Option<FShut>>,
    _t: PhantomData<Req>,
}

impl<F, Fut, Req, Res, Err> FnService<F, Fut, Req, Res, Err>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    pub(crate) fn new(f: F) -> Self {
        Self {
            f,
            f_shutdown: Cell::new(Some(fn_shutdown)),
            _t: PhantomData,
        }
    }

    /// Set function that get called oin poll_shutdown method of Service trait.
    pub fn on_shutdown<FShut>(self, f: FShut) -> FnService<F, Fut, Req, Res, Err, FShut>
    where
        FShut: FnOnce(),
    {
        FnService {
            f: self.f,
            f_shutdown: Cell::new(Some(f)),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Req, Res, Err, FShut> Clone for FnService<F, Fut, Req, Res, Err, FShut>
where
    F: Fn(Req) -> Fut + Clone,
    FShut: FnOnce() + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    #[inline]
    fn clone(&self) -> Self {
        let f = self.f_shutdown.take();
        self.f_shutdown.set(f.clone());

        Self {
            f: self.f.clone(),
            f_shutdown: Cell::new(f),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Req, Res, Err, FShut> Service for FnService<F, Fut, Req, Res, Err, FShut>
where
    F: Fn(Req) -> Fut,
    FShut: FnOnce(),
    Fut: Future<Output = Result<Res, Err>>,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;
    type Future = Fut;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(&self, _: &mut Context<'_>, _: bool) -> Poll<()> {
        if let Some(f) = self.f_shutdown.take() {
            (f)()
        }
        Poll::Ready(())
    }

    #[inline]
    fn call(&self, req: Req) -> Self::Future {
        (self.f)(req)
    }
}

impl<F, Fut, Req, Res, Err> IntoService<FnService<F, Fut, Req, Res, Err>> for F
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    #[inline]
    fn into_service(self) -> FnService<F, Fut, Req, Res, Err> {
        FnService::new(self)
    }
}

pub struct FnServiceFactory<F, Fut, Req, Res, Err, Cfg, FShut = fn()>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    f: F,
    f_shutdown: Cell<Option<FShut>>,
    _t: PhantomData<(Req, Cfg)>,
}

impl<F, Fut, Req, Res, Err, Cfg> FnServiceFactory<F, Fut, Req, Res, Err, Cfg>
where
    F: Fn(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    fn new(f: F) -> Self {
        FnServiceFactory {
            f,
            f_shutdown: Cell::new(Some(fn_shutdown)),
            _t: PhantomData,
        }
    }

    /// Set function that get called oin poll_shutdown method of Service trait.
    pub fn on_shutdown<FShut>(
        self,
        f: FShut,
    ) -> FnServiceFactory<F, Fut, Req, Res, Err, Cfg, FShut>
    where
        FShut: FnOnce(),
    {
        FnServiceFactory {
            f: self.f,
            f_shutdown: Cell::new(Some(f)),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Req, Res, Err, Cfg, FShut> Clone
    for FnServiceFactory<F, Fut, Req, Res, Err, Cfg, FShut>
where
    F: Fn(Req) -> Fut + Clone,
    FShut: FnOnce() + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    #[inline]
    fn clone(&self) -> Self {
        let f = self.f_shutdown.take();
        self.f_shutdown.set(f.clone());

        Self {
            f: self.f.clone(),
            f_shutdown: Cell::new(f),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Req, Res, Err, FShut> Service
    for FnServiceFactory<F, Fut, Req, Res, Err, (), FShut>
where
    F: Fn(Req) -> Fut,
    FShut: FnOnce(),
    Fut: Future<Output = Result<Res, Err>>,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;
    type Future = Fut;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(&self, _: &mut Context<'_>, _: bool) -> Poll<()> {
        if let Some(f) = self.f_shutdown.take() {
            (f)()
        }
        Poll::Ready(())
    }

    #[inline]
    fn call(&self, req: Self::Request) -> Self::Future {
        (self.f)(req)
    }
}

impl<F, Fut, Req, Res, Err, Cfg, FShut> ServiceFactory
    for FnServiceFactory<F, Fut, Req, Res, Err, Cfg, FShut>
where
    F: Fn(Req) -> Fut + Clone,
    FShut: FnOnce() + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;

    type Config = Cfg;
    type Service = FnService<F, Fut, Req, Res, Err, FShut>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn new_service(&self, _: Cfg) -> Self::Future {
        let f = self.f_shutdown.take();
        self.f_shutdown.set(f.clone());

        Ready::Ok(FnService {
            f: self.f.clone(),
            f_shutdown: Cell::new(f),
            _t: PhantomData,
        })
    }
}

impl<F, Fut, Req, Res, Err, Cfg>
    IntoServiceFactory<FnServiceFactory<F, Fut, Req, Res, Err, Cfg>> for F
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
pub struct FnServiceConfig<F, Fut, Cfg, Srv, Err>
where
    F: Fn(Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service,
{
    f: F,
    _t: PhantomData<(Fut, Cfg, Srv, Err)>,
}

impl<F, Fut, Cfg, Srv, Err> FnServiceConfig<F, Fut, Cfg, Srv, Err>
where
    F: Fn(Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service,
{
    fn new(f: F) -> Self {
        FnServiceConfig { f, _t: PhantomData }
    }
}

impl<F, Fut, Cfg, Srv, Err> Clone for FnServiceConfig<F, Fut, Cfg, Srv, Err>
where
    F: Fn(Cfg) -> Fut + Clone,
    Fut: Future<Output = Result<Srv, Err>>,
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

impl<F, Fut, Cfg, Srv, Err> ServiceFactory for FnServiceConfig<F, Fut, Cfg, Srv, Err>
where
    F: Fn(Cfg) -> Fut,
    Fut: Future<Output = Result<Srv, Err>>,
    Srv: Service,
{
    type Request = Srv::Request;
    type Response = Srv::Response;
    type Error = Srv::Error;

    type Config = Cfg;
    type Service = Srv;
    type InitError = Err;
    type Future = Fut;

    #[inline]
    fn new_service(&self, cfg: Cfg) -> Self::Future {
        (self.f)(cfg)
    }
}

/// Converter for `Fn() -> Future<Service>` fn
pub struct FnServiceNoConfig<F, C, S, R, E>
where
    F: Fn() -> R,
    S: Service,
    R: Future<Output = Result<S, E>>,
{
    f: F,
    _t: PhantomData<C>,
}

impl<F, C, S, R, E> FnServiceNoConfig<F, C, S, R, E>
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    fn new(f: F) -> Self {
        Self { f, _t: PhantomData }
    }
}

impl<F, C, S, R, E> ServiceFactory for FnServiceNoConfig<F, C, S, R, E>
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;
    type Config = C;
    type InitError = E;
    type Future = R;

    #[inline]
    fn new_service(&self, _: C) -> Self::Future {
        (self.f)()
    }
}

impl<F, C, S, R, E> Clone for FnServiceNoConfig<F, C, S, R, E>
where
    F: Fn() -> R + Clone,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    #[inline]
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

impl<F, C, S, R, E> IntoServiceFactory<FnServiceNoConfig<F, C, S, R, E>> for F
where
    F: Fn() -> R,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    #[inline]
    fn into_factory(self) -> FnServiceNoConfig<F, C, S, R, E> {
        FnServiceNoConfig::new(self)
    }
}

pub struct FnMutService<F, Fut, Req, Res, Err>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    f: RefCell<F>,
    _t: PhantomData<Req>,
}

impl<F, Fut, Req, Res, Err> FnMutService<F, Fut, Req, Res, Err>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    pub(crate) fn new(f: F) -> Self {
        Self {
            f: RefCell::new(f),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Req, Res, Err> Clone for FnMutService<F, Fut, Req, Res, Err>
where
    F: FnMut(Req) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    #[inline]
    fn clone(&self) -> Self {
        Self::new(self.f.borrow().clone())
    }
}

impl<F, Fut, Req, Res, Err> Service for FnMutService<F, Fut, Req, Res, Err>
where
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;
    type Future = Fut;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: Req) -> Self::Future {
        (&mut *self.f.borrow_mut())(req)
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::lazy;
    use std::{rc::Rc, task::Poll};

    use super::*;
    use crate::{Service, ServiceFactory};

    #[ntex::test]
    async fn test_fn_service() {
        let shutdown = Rc::new(RefCell::new(false));
        let new_srv = fn_service(|()| async { Ok::<_, ()>("srv") })
            .on_shutdown(|| {
                *shutdown.borrow_mut() = true;
            })
            .clone();

        let srv = new_srv.new_service(()).await.unwrap();
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
        assert!(*shutdown.borrow());
    }

    #[ntex::test]
    async fn test_fn_mut_service() {
        let srv = fn_mut_service(|()| async { Ok::<_, ()>("srv") }).clone();

        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
    }

    #[ntex::test]
    async fn test_fn_service_service() {
        let shutdown = Rc::new(RefCell::new(false));
        let srv = fn_service(|()| async { Ok::<_, ()>("srv") })
            .on_shutdown(|| {
                *shutdown.borrow_mut() = true;
            })
            .clone();

        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        assert_eq!(
            lazy(|cx| srv.poll_shutdown(cx, false)).await,
            Poll::Ready(())
        );
        assert!(*shutdown.borrow());
    }

    #[ntex::test]
    async fn test_fn_service_with_config() {
        let new_srv = fn_factory_with_config(|cfg: usize| async move {
            Ok::<_, ()>(fn_service(
                move |()| async move { Ok::<_, ()>(("srv", cfg)) },
            ))
        })
        .clone();

        let srv = new_srv.new_service(1).await.unwrap();
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", 1));
    }
}
