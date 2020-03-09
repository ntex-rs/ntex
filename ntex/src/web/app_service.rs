use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_router::{Path, ResourceDef, ResourceInfo, Router, Url};
use futures::future::{ok, FutureExt, LocalBoxFuture};

use crate::http::{Extensions, Request, Response};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::{fn_service, Service, ServiceFactory};

use super::config::{AppConfig, AppService};
use super::data::DataFactory;
use super::error::WebError;
use super::guard::Guard;
use super::request::{HttpRequest, HttpRequestPool};
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, WebRequest, WebResponse};

type Guards = Vec<Box<dyn Guard>>;
type HttpService<Err> = BoxService<WebRequest, WebResponse, WebError<Err>>;
type HttpNewService<Err> =
    BoxServiceFactory<(), WebRequest, WebResponse, WebError<Err>, ()>;
type BoxResponse<Err> = LocalBoxFuture<'static, Result<WebResponse, WebError<Err>>>;
type FnDataFactory =
    Box<dyn Fn() -> LocalBoxFuture<'static, Result<Box<dyn DataFactory>, ()>>>;

/// Service factory to convert `Request` to a `WebRequest<S>`.
/// It also executes data factories.
pub struct AppInit<T, B, Err>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest,
        Response = WebResponse<B>,
        Error = WebError<Err>,
        InitError = (),
    >,
{
    pub(super) endpoint: T,
    pub(super) extensions: RefCell<Option<Extensions>>,
    pub(super) data: Rc<Vec<Box<dyn DataFactory>>>,
    pub(super) data_factories: Rc<Vec<FnDataFactory>>,
    pub(super) services: Rc<RefCell<Vec<Box<dyn AppServiceFactory<Err>>>>>,
    pub(super) default: Option<Rc<HttpNewService<Err>>>,
    pub(super) factory_ref: Rc<RefCell<Option<AppRoutingFactory<Err>>>>,
    pub(super) external: RefCell<Vec<ResourceDef>>,
}

impl<T, B, Err> ServiceFactory for AppInit<T, B, Err>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest,
        Response = WebResponse<B>,
        Error = WebError<Err>,
        InitError = (),
    >,
    Err: 'static,
{
    type Config = AppConfig;
    type Request = Request;
    type Response = WebResponse<B>;
    type Error = T::Error;
    type InitError = T::InitError;
    type Service = AppInitService<T::Service, B, Err>;
    type Future = AppInitResult<T, B, Err>;

    fn new_service(&self, config: AppConfig) -> Self::Future {
        // update resource default service
        let default = self.default.clone().unwrap_or_else(|| {
            Rc::new(boxed::factory(fn_service(|req: WebRequest| {
                ok(req.into_response(Response::NotFound().finish()))
            })))
        });

        // App config
        let mut config = AppService::new(config, default.clone(), self.data.clone());

        // register services
        std::mem::replace(&mut *self.services.borrow_mut(), Vec::new())
            .into_iter()
            .for_each(|mut srv| srv.register(&mut config));

        let mut rmap = ResourceMap::new(ResourceDef::new(""));

        let (config, services) = config.into_services();

        // complete pipeline creation
        *self.factory_ref.borrow_mut() = Some(AppRoutingFactory {
            default,
            services: Rc::new(
                services
                    .into_iter()
                    .map(|(mut rdef, srv, guards, nested)| {
                        rmap.add(&mut rdef, nested);
                        (rdef, srv, RefCell::new(guards))
                    })
                    .collect(),
            ),
        });

        // external resources
        for mut rdef in std::mem::replace(&mut *self.external.borrow_mut(), Vec::new()) {
            rmap.add(&mut rdef, None);
        }

        // complete ResourceMap tree creation
        let rmap = Rc::new(rmap);
        rmap.finish(rmap.clone());

        AppInitResult {
            endpoint: None,
            endpoint_fut: self.endpoint.new_service(()),
            data: self.data.clone(),
            data_factories: Vec::new(),
            data_factories_fut: self.data_factories.iter().map(|f| f()).collect(),
            extensions: Some(
                self.extensions
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(Extensions::new),
            ),
            config,
            rmap,
            _t: PhantomData,
        }
    }
}

#[pin_project::pin_project]
pub struct AppInitResult<T, B, Err>
where
    T: ServiceFactory,
{
    endpoint: Option<T::Service>,
    #[pin]
    endpoint_fut: T::Future,
    rmap: Rc<ResourceMap>,
    config: AppConfig,
    data: Rc<Vec<Box<dyn DataFactory>>>,
    data_factories: Vec<Box<dyn DataFactory>>,
    data_factories_fut: Vec<LocalBoxFuture<'static, Result<Box<dyn DataFactory>, ()>>>,
    extensions: Option<Extensions>,
    _t: PhantomData<(B, Err)>,
}

impl<T, B, Err> Future for AppInitResult<T, B, Err>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest,
        Response = WebResponse<B>,
        Error = WebError<Err>,
        InitError = (),
    >,
{
    type Output = Result<AppInitService<T::Service, B, Err>, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // async data factories
        let mut idx = 0;
        while idx < this.data_factories_fut.len() {
            match Pin::new(&mut this.data_factories_fut[idx]).poll(cx)? {
                Poll::Ready(f) => {
                    this.data_factories.push(f);
                    let _ = this.data_factories_fut.remove(idx);
                }
                Poll::Pending => idx += 1,
            }
        }

        if this.endpoint.is_none() {
            if let Poll::Ready(srv) = this.endpoint_fut.poll(cx)? {
                *this.endpoint = Some(srv);
            }
        }

        if this.endpoint.is_some() && this.data_factories_fut.is_empty() {
            // create app data container
            let mut data = this.extensions.take().unwrap();
            for f in this.data.iter() {
                f.create(&mut data);
            }

            for f in this.data_factories.iter() {
                f.create(&mut data);
            }

            Poll::Ready(Ok(AppInitService {
                service: this.endpoint.take().unwrap(),
                rmap: this.rmap.clone(),
                config: this.config.clone(),
                data: Rc::new(data),
                pool: HttpRequestPool::create(),
            }))
        } else {
            Poll::Pending
        }
    }
}

/// Service to convert `Request` to a `WebRequest<S>`
pub struct AppInitService<T, B, Err>
where
    T: Service<Request = WebRequest, Response = WebResponse<B>, Error = WebError<Err>>,
{
    service: T,
    rmap: Rc<ResourceMap>,
    config: AppConfig,
    data: Rc<Extensions>,
    pool: &'static HttpRequestPool,
}

impl<T, B, Err> Service for AppInitService<T, B, Err>
where
    T: Service<Request = WebRequest, Response = WebResponse<B>, Error = WebError<Err>>,
{
    type Request = Request;
    type Response = WebResponse<B>;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let (head, payload) = req.into_parts();

        let req = if let Some(mut req) = self.pool.get_request() {
            let inner = Rc::get_mut(&mut req.0).unwrap();
            inner.path.get_mut().update(&head.uri);
            inner.path.reset();
            inner.head = head;
            inner.payload = payload;
            inner.app_data = self.data.clone();
            req
        } else {
            HttpRequest::new(
                Path::new(Url::new(head.uri.clone())),
                head,
                payload,
                self.rmap.clone(),
                self.config.clone(),
                self.data.clone(),
                self.pool,
            )
        };
        self.service.call(WebRequest::new(req))
    }
}

impl<T, B, Err> Drop for AppInitService<T, B, Err>
where
    T: Service<Request = WebRequest, Response = WebResponse<B>, Error = WebError<Err>>,
{
    fn drop(&mut self) {
        self.pool.clear();
    }
}

pub struct AppRoutingFactory<Err> {
    services: Rc<Vec<(ResourceDef, HttpNewService<Err>, RefCell<Option<Guards>>)>>,
    default: Rc<HttpNewService<Err>>,
}

impl<Err: 'static> ServiceFactory for AppRoutingFactory<Err> {
    type Config = ();
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type InitError = ();
    type Service = AppRouting<Err>;
    type Future = AppRoutingFactoryResponse<Err>;

    fn new_service(&self, _: ()) -> Self::Future {
        AppRoutingFactoryResponse {
            fut: self
                .services
                .iter()
                .map(|(path, service, guards)| {
                    CreateAppRoutingItem::Future(
                        Some(path.clone()),
                        guards.borrow_mut().take(),
                        service.new_service(()).boxed_local(),
                    )
                })
                .collect(),
            default: None,
            default_fut: Some(self.default.new_service(())),
        }
    }
}

type HttpServiceFut<Err> = LocalBoxFuture<'static, Result<HttpService<Err>, ()>>;

/// Create app service
#[doc(hidden)]
pub struct AppRoutingFactoryResponse<Err> {
    fut: Vec<CreateAppRoutingItem<Err>>,
    default: Option<HttpService<Err>>,
    default_fut: Option<LocalBoxFuture<'static, Result<HttpService<Err>, ()>>>,
}

enum CreateAppRoutingItem<Err> {
    Future(Option<ResourceDef>, Option<Guards>, HttpServiceFut<Err>),
    Service(ResourceDef, Option<Guards>, HttpService<Err>),
}

impl<Err> Future for AppRoutingFactoryResponse<Err> {
    type Output = Result<AppRouting<Err>, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut done = true;

        if let Some(ref mut fut) = self.default_fut {
            match Pin::new(fut).poll(cx)? {
                Poll::Ready(default) => self.default = Some(default),
                Poll::Pending => done = false,
            }
        }

        // poll http services
        for item in &mut self.fut {
            let res = match item {
                CreateAppRoutingItem::Future(
                    ref mut path,
                    ref mut guards,
                    ref mut fut,
                ) => match Pin::new(fut).poll(cx) {
                    Poll::Ready(Ok(service)) => {
                        Some((path.take().unwrap(), guards.take(), service))
                    }
                    Poll::Ready(Err(_)) => return Poll::Ready(Err(())),
                    Poll::Pending => {
                        done = false;
                        None
                    }
                },
                CreateAppRoutingItem::Service(_, _, _) => continue,
            };

            if let Some((path, guards, service)) = res {
                *item = CreateAppRoutingItem::Service(path, guards, service);
            }
        }

        if done {
            let router = self
                .fut
                .drain(..)
                .fold(Router::build(), |mut router, item| {
                    match item {
                        CreateAppRoutingItem::Service(path, guards, service) => {
                            router.rdef(path, service).2 = guards;
                        }
                        CreateAppRoutingItem::Future(_, _, _) => unreachable!(),
                    }
                    router
                });
            Poll::Ready(Ok(AppRouting {
                ready: None,
                router: router.finish(),
                default: self.default.take(),
            }))
        } else {
            Poll::Pending
        }
    }
}

pub struct AppRouting<Err> {
    router: Router<HttpService<Err>, Guards>,
    ready: Option<(WebRequest, ResourceInfo)>,
    default: Option<HttpService<Err>>,
}

impl<Err: 'static> Service for AppRouting<Err> {
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type Future = BoxResponse<Err>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.ready.is_none() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&mut self, mut req: WebRequest) -> Self::Future {
        let res = self.router.recognize_mut_checked(&mut req, |req, guards| {
            if let Some(ref guards) = guards {
                for f in guards {
                    if !f.check(req.head()) {
                        return false;
                    }
                }
            }
            true
        });

        if let Some((srv, _info)) = res {
            srv.call(req)
        } else if let Some(ref mut default) = self.default {
            default.call(req)
        } else {
            let req = req.into_parts().0;
            ok(WebResponse::new(req, Response::NotFound().finish())).boxed_local()
        }
    }
}

/// Wrapper service for routing
pub struct AppEntry<Err> {
    factory: Rc<RefCell<Option<AppRoutingFactory<Err>>>>,
}

impl<Err> AppEntry<Err> {
    pub fn new(factory: Rc<RefCell<Option<AppRoutingFactory<Err>>>>) -> Self {
        AppEntry { factory }
    }
}

impl<Err: 'static> ServiceFactory for AppEntry<Err> {
    type Config = ();
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type InitError = ();
    type Service = AppRouting<Err>;
    type Future = AppRoutingFactoryResponse<Err>;

    fn new_service(&self, _: ()) -> Self::Future {
        self.factory.borrow_mut().as_mut().unwrap().new_service(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use crate::service::Service;
    use crate::web::test::{init_service, TestRequest};
    use crate::web::{self, App, HttpResponse};

    struct DropData(Arc<AtomicBool>);

    impl Drop for DropData {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[actix_rt::test]
    async fn test_drop_data() {
        let data = Arc::new(AtomicBool::new(false));

        {
            let mut app =
                init_service(App::new().data(DropData(data.clone())).service(
                    web::resource("/test").to(|| async { HttpResponse::Ok() }),
                ))
                .await;
            let req = TestRequest::with_uri("/test").to_request();
            let _ = app.call(req).await.unwrap();
        }
        assert!(data.load(Ordering::Relaxed));
    }
}
