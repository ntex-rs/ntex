use std::{convert::Infallible, fmt, marker::PhantomData};

use crate::{Ctx, IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Converts an asynchronous function into a [`Service`].
#[inline]
pub fn fn_service<F, Req, Res, Err>(f: F) -> FnService<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    FnService { f, _t: PhantomData }
}

/// Converts a state-aware asynchronous function into a [`Service`].
///
/// This service accesses the pipeline state via the first `&St` parameter.
#[inline]
pub fn fn_service_st<F, St, Req, Res, Err>(f: F) -> FnServiceSt<F, St, Req, Res, Err>
where
    F: AsyncFn(&St, Req) -> Result<Res, Err>,
{
    FnServiceSt { f, _t: PhantomData }
}

// ====================== FnService =======================

/// A [`Service`] backed by an asynchronous function.
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

impl<F, St, Req, Res, Err> Service<St, Req> for FnService<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err>,
{
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<Res, Err> {
        (self.f)(req).await
    }
}

impl<F, St, Req, Res, Err> IntoServiceFactory<FnServiceFactory<F, Req, Res, Err>, St, Req>
    for FnService<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn into_factory(self) -> FnServiceFactory<F, Req, Res, Err> {
        FnServiceFactory {
            f: self.f,
            _t: PhantomData,
        }
    }
}

impl<F, St, Req, Res, Err> IntoService<FnService<F, Req, Res, Err>, St, Req> for F
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

/// A state-aware [`Service`] backed by an asynchronous function.
///
/// This service accesses the pipeline state via the first `&St` parameter.
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

impl<F, St, Req, Res, Err> Service<St, Req> for FnServiceSt<F, St, Req, Res, Err>
where
    F: AsyncFn(&St, Req) -> Result<Res, Err>,
{
    type Res = Res;
    type Error = Err;

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Res, Err> {
        (self.f)(ctx.st(), req).await
    }
}

impl<F, St, Req, Res, Err> IntoServiceFactory<FnServiceStFactory<F, St, Req, Res, Err>, St, Req>
    for FnServiceSt<F, St, Req, Res, Err>
where
    F: AsyncFn(&St, Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn into_factory(self) -> FnServiceStFactory<F, St, Req, Res, Err> {
        FnServiceStFactory {
            f: self.f,
            ph: PhantomData,
        }
    }
}

impl<F, St, Req, Res, Err> IntoService<FnServiceSt<F, St, Req, Res, Err>, St, Req> for F
where
    F: AsyncFn(&St, Req) -> Result<Res, Err>,
{
    #[inline]
    fn into_service(self) -> FnServiceSt<F, St, Req, Res, Err> {
        FnServiceSt {
            f: self,
            _t: PhantomData,
        }
    }
}

// ---------------------------- FnServiceFactory ------------------------

/// A service factory backed by a clonable asynchronous service function.
pub struct FnServiceFactory<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    f: F,
    _t: PhantomData<(Req,)>,
}

impl<F, Req, Res, Err> FnServiceFactory<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    fn new(f: F) -> Self {
        FnServiceFactory { f, _t: PhantomData }
    }
}

impl<F, Req, Res, Err> Clone for FnServiceFactory<F, Req, Res, Err>
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

impl<F, Req, Res, Err> fmt::Debug for FnServiceFactory<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceFactory")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, St, Req, Res, Err> ServiceFactory<St, Req> for FnServiceFactory<F, Req, Res, Err>
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    type Res = Res;
    type Error = Err;

    type Service = FnService<F, Req, Res, Err>;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(FnService {
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

impl<St, F, Req, Res, Err> IntoServiceFactory<FnServiceFactory<F, Req, Res, Err>, St, Req> for F
where
    F: AsyncFn(Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn into_factory(self) -> FnServiceFactory<F, Req, Res, Err> {
        FnServiceFactory::new(self)
    }
}

// ========================= FnServiceStFactory =======================

/// A service factory backed by a clonable, state-aware asynchronous function.
pub struct FnServiceStFactory<F, St, Req, Res, Err>
where
    F: AsyncFn(&St, Req) -> Result<Res, Err> + Clone,
{
    f: F,
    ph: PhantomData<(St, Req, Res, Err)>,
}

impl<F, St, Req, Res, Err> Clone for FnServiceStFactory<F, St, Req, Res, Err>
where
    F: AsyncFn(&St, Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            ph: PhantomData,
        }
    }
}

impl<F, St, Req, Res, Err> fmt::Debug for FnServiceStFactory<F, St, Req, Res, Err>
where
    F: AsyncFn(&St, Req) -> Result<Res, Err> + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnServiceStFactory")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, St, Req, Res, Err> ServiceFactory<St, Req> for FnServiceStFactory<F, St, Req, Res, Err>
where
    F: AsyncFn(&St, Req) -> Result<Res, Err> + Clone,
{
    type Res = Res;
    type Error = Err;

    type Service = FnServiceSt<F, St, Req, Res, Err>;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(FnServiceSt {
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

impl<F, St, Req, Res, Err> IntoServiceFactory<FnServiceStFactory<F, St, Req, Res, Err>, St, Req>
    for F
where
    F: AsyncFn(&St, Req) -> Result<Res, Err> + Clone,
{
    #[inline]
    fn into_factory(self) -> FnServiceStFactory<F, St, Req, Res, Err> {
        FnServiceStFactory {
            f: self,
            ph: PhantomData,
        }
    }
}

// ========================= FnFactory ==================================

#[inline]
/// Converts an asynchronous constructor function into a [`ServiceFactory`].
///
/// Any function with the form
/// `AsyncFn(&State) -> Result<Service, InitError>` can act as a factory.
///
/// # Example
///
/// ```rust
/// use std::io;
/// use ntex_service::{factory, fn_factory, fn_service, Pipeline, Service, ServiceFactory};
///
/// #[ntex::main]
/// async fn main() -> io::Result<()> {
///     // Create a factory that uses the state to configure each service.
///     let fac = fn_factory(async |y: &usize| {
///         let y = *y;
///         Ok::<_, io::Error>(fn_service(move |x: usize| async move { Ok::<_, io::Error>(x * y) }))
///     });
///
///     // Construct a service with the supplied state.
///     let srv = Pipeline::new(12, factory(fac).create(&10).await?);
///
///     let result = srv.call(10).await?;
///     assert_eq!(result, 100);
///
///     println!("10 * 10 = {}", result);
///     Ok(())
/// }
/// ```
pub fn fn_factory<F, S, St, Req, Err>(f: F) -> FnFactory<F, S, St, Req, Err>
where
    F: AsyncFn(&St) -> Result<S, Err>,
    S: Service<St, Req>,
{
    FnFactory { f, _t: PhantomData }
}

/// A [`ServiceFactory`] backed by an asynchronous constructor function.
pub struct FnFactory<F, S, St, Req, Err>
where
    F: AsyncFn(&St) -> Result<S, Err>,
    S: Service<St, Req>,
{
    f: F,
    _t: PhantomData<(S, St, Req, Err)>,
}

impl<F, S, St, Req, Err> ServiceFactory<St, Req> for FnFactory<F, S, St, Req, Err>
where
    F: AsyncFn(&St) -> Result<S, Err>,
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    type Service = S;
    type InitError = Err;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        (self.f)(st).await
    }
}

impl<F, S, St, Req, Err> Clone for FnFactory<F, S, St, Req, Err>
where
    F: AsyncFn(&St) -> Result<S, Err> + Clone,
    S: Service<St, Req>,
{
    #[inline]
    fn clone(&self) -> Self {
        FnFactory {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, S, St, Req, Err> fmt::Debug for FnFactory<F, S, St, Req, Err>
where
    F: AsyncFn(&St) -> Result<S, Err>,
    S: Service<St, Req>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnFactory")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, S, St, Req, Err> IntoServiceFactory<FnFactory<F, S, St, Req, Err>, St, Req> for F
where
    F: AsyncFn(&St) -> Result<S, Err>,
    S: Service<St, Req>,
{
    #[inline]
    fn into_factory(self) -> FnFactory<F, S, St, Req, Err> {
        FnFactory {
            f: self,
            _t: PhantomData,
        }
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

        let srv = Pipeline::new((), new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let new_srv = fn_service(async |()| Ok::<_, ()>("srv"));
        let srv = Pipeline::new((), new_srv.clone());
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

        let srv = Pipeline::new((), factory(new_srv).create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let new_srv = fn_service(async |()| Ok::<_, ()>("srv")).clone();
        let srv = Pipeline::new((), new_srv.clone());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_fn_service_service() {
        let srv = Pipeline::new(
            (),
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
    async fn test_fn_factory() {
        let new_srv = factory(fn_factory(async move |cfg: &usize| {
            let cfg = *cfg;
            Ok::<_, ()>(fn_service(async move |()| Ok::<_, ()>(("srv", cfg))))
        }))
        .clone();

        let srv = Pipeline::new(1, new_srv.create(&1).await.unwrap());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", 1));
    }

    #[ntex::test]
    async fn test_fn_service_st() {
        let new_srv = factory(async |(): &(), ()| Ok::<_, ()>("srv")).clone();
        let _ = format!("{new_srv:?}");

        let srv = Pipeline::new((), new_srv.create(&()).await.unwrap());
        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        let new_srv = fn_service(async |()| Ok::<_, ()>("srv"));
        let srv = Pipeline::new((), new_srv.clone());
        let res = srv.call(()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        let _ = format!("{srv:?}");

        assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
    }
}
