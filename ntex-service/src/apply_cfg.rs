use std::{
    future::Future, marker::PhantomData, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use crate::{Service, ServiceFactory};

/// Convert `Fn(Config, &mut Service1) -> Future<Service2>` fn to a service factory
pub fn apply_cfg<F, C, T, R, S, E>(
    srv: T,
    f: F,
) -> impl ServiceFactory<
    Config = C,
    Request = S::Request,
    Response = S::Response,
    Error = S::Error,
    Service = S,
    InitError = E,
    Future = R,
> + Clone
where
    F: Fn(C, &T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    ApplyConfigService {
        srv: Rc::new((srv, f)),
        _t: PhantomData,
    }
}

/// Convert `Fn(Config, &Service1) -> Future<Service2>` fn to a service factory
///
/// Service1 get constructed from `T` factory.
pub fn apply_cfg_factory<F, C, T, R, S>(
    factory: T,
    f: F,
) -> impl ServiceFactory<
    Config = C,
    Request = S::Request,
    Response = S::Response,
    Error = S::Error,
    Service = S,
    InitError = T::InitError,
> + Clone
where
    F: Fn(C, &T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    ApplyConfigServiceFactory {
        srv: Rc::new((factory, f)),
        _t: PhantomData,
    }
}

/// Convert `Fn(Config, &Server) -> Future<Service>` fn to NewService\
struct ApplyConfigService<F, C, T, R, S, E>
where
    F: Fn(C, &T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    srv: Rc<(T, F)>,
    _t: PhantomData<(C, R, S)>,
}

impl<F, C, T, R, S, E> Clone for ApplyConfigService<F, C, T, R, S, E>
where
    F: Fn(C, &T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    fn clone(&self) -> Self {
        ApplyConfigService {
            srv: self.srv.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, C, T, R, S, E> ServiceFactory for ApplyConfigService<F, C, T, R, S, E>
where
    F: Fn(C, &T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    type Config = C;
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = E;
    type Future = R;

    fn new_service(&self, cfg: C) -> Self::Future {
        let srv = self.srv.as_ref();
        (srv.1)(cfg, &srv.0)
    }
}

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService
struct ApplyConfigServiceFactory<F, C, T, R, S>
where
    F: Fn(C, &T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    srv: Rc<(T, F)>,
    _t: PhantomData<(C, R, S)>,
}

impl<F, C, T, R, S> Clone for ApplyConfigServiceFactory<F, C, T, R, S>
where
    F: Fn(C, &T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    fn clone(&self) -> Self {
        Self {
            srv: self.srv.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, C, T, R, S> ServiceFactory for ApplyConfigServiceFactory<F, C, T, R, S>
where
    F: Fn(C, &T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    type Config = C;
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = T::InitError;
    type Future = ApplyConfigServiceFactoryResponse<F, C, T, R, S>;

    fn new_service(&self, cfg: C) -> Self::Future {
        ApplyConfigServiceFactoryResponse {
            cfg: Some(cfg),
            store: self.srv.clone(),
            state: State::A {
                fut: self.srv.as_ref().0.new_service(()),
            },
        }
    }
}

pin_project_lite::pin_project! {
    struct ApplyConfigServiceFactoryResponse<F, C, T, R, S>
    where
        F: Fn(C, &T::Service) -> R,
        T: ServiceFactory<Config = ()>,
        T::InitError: From<T::Error>,
        R: Future<Output = Result<S, T::InitError>>,
        S: Service,
    {
        cfg: Option<C>,
        store: Rc<(T, F)>,
        #[pin]
        state: State<T, R, S>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum State<T, R, S>
    where
        T: ServiceFactory<Config = ()>,
        T::InitError: From<T::Error>,
        R: Future<Output = Result<S, T::InitError>>,
        S: Service,
    {
        A { #[pin] fut: T::Future },
        B { srv: T::Service },
        C { #[pin] fut: R },
    }
}

impl<F, C, T, R, S> Future for ApplyConfigServiceFactoryResponse<F, C, T, R, S>
where
    F: Fn(C, &T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    type Output = Result<S, T::InitError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            StateProject::A { fut } => match fut.poll(cx)? {
                Poll::Pending => Poll::Pending,
                Poll::Ready(srv) => {
                    this.state.set(State::B { srv });
                    self.poll(cx)
                }
            },
            StateProject::B { srv } => match srv.poll_ready(cx)? {
                Poll::Ready(_) => {
                    let fut = (this.store.as_ref().1)(this.cfg.take().unwrap(), srv);
                    this.state.set(State::C { fut });
                    self.poll(cx)
                }
                Poll::Pending => Poll::Pending,
            },
            StateProject::C { fut } => fut.poll(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::Ready;
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{fn_service, Service};

    #[ntex::test]
    async fn test_apply() {
        let item = Rc::new(Cell::new(10usize));
        let item2 = item.clone();

        let srv = apply_cfg(
            fn_service(move |item: usize| Ready::<_, ()>::Ok(item + 1)),
            move |_: (), srv| {
                let id = item2.get();
                let fut = srv.call(id);
                let item = item2.clone();

                async move {
                    item.set(fut.await.unwrap());
                    Ok::<_, ()>(fn_service(|id: usize| Ready::<_, ()>::Ok(id * 2)))
                }
            },
        )
        .clone()
        .new_service(())
        .await
        .unwrap();

        assert_eq!(srv.call(10usize).await.unwrap(), 20);
        assert_eq!(item.get(), 11);
    }

    #[ntex::test]
    async fn test_apply_factory() {
        let item = Rc::new(Cell::new(10usize));
        let item2 = item.clone();

        let srv = apply_cfg_factory(
            fn_service(move |item: usize| Ready::<_, ()>::Ok(item + 1)),
            move |_: (), srv| {
                let id = item2.get();
                let fut = srv.call(id);
                let item = item2.clone();

                async move {
                    item.set(fut.await.unwrap());
                    Ok::<_, ()>(fn_service(|id: usize| Ready::<_, ()>::Ok(id * 2)))
                }
            },
        )
        .clone()
        .new_service(())
        .await
        .unwrap();

        assert_eq!(srv.call(10usize).await.unwrap(), 20);
        assert_eq!(item.get(), 11);
    }
}
