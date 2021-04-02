use std::task::{Context, Poll};
use std::{cell::RefCell, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, ResourceInfo, Router};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::util::Extensions;
use crate::{fn_service, Service, ServiceFactory};

use super::config::AppConfig;
use super::error::ErrorRenderer;
use super::guard::Guard;
use super::httprequest::{HttpRequest, HttpRequestPool};
use super::request::WebRequest;
use super::response::WebResponse;
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, WebServiceConfig};
use super::types::data::DataFactory;

type Guards = Vec<Box<dyn Guard>>;
type HttpService<Err: ErrorRenderer> =
    BoxService<WebRequest<Err>, WebResponse, Err::Container>;
type HttpNewService<Err: ErrorRenderer> =
    BoxServiceFactory<(), WebRequest<Err>, WebResponse, Err::Container, ()>;
type BoxResponse<Err: ErrorRenderer> =
    Pin<Box<dyn Future<Output = Result<WebResponse, Err::Container>>>>;
type FnDataFactory =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Box<dyn DataFactory>, ()>>>>>;

/// Service factory to convert `Request` to a `WebRequest<S>`.
/// It also executes data factories.
pub struct AppFactory<T, Err: ErrorRenderer>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest<Err>,
        Response = WebResponse,
        Error = Err::Container,
        InitError = (),
    >,
    T::Future: 'static,
    Err: ErrorRenderer,
{
    pub(super) endpoint: T,
    pub(super) extensions: RefCell<Option<Extensions>>,
    pub(super) data: Rc<Vec<Box<dyn DataFactory>>>,
    pub(super) data_factories: Rc<Vec<FnDataFactory>>,
    pub(super) services: Rc<RefCell<Vec<Box<dyn AppServiceFactory<Err>>>>>,
    pub(super) default: Option<Rc<HttpNewService<Err>>>,
    pub(super) factory_ref: Rc<RefCell<Option<AppRoutingFactory<Err>>>>,
    pub(super) external: RefCell<Vec<ResourceDef>>,
    pub(super) case_insensitive: bool,
}

impl<T, Err> ServiceFactory for AppFactory<T, Err>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest<Err>,
        Response = WebResponse,
        Error = Err::Container,
        InitError = (),
    >,
    T::Future: 'static,
    Err: ErrorRenderer,
{
    type Config = AppConfig;
    type Request = Request;
    type Response = WebResponse;
    type Error = T::Error;
    type InitError = T::InitError;
    type Service = AppFactoryService<T::Service, Err>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, config: AppConfig) -> Self::Future {
        // update resource default service
        let default = self.default.clone().unwrap_or_else(|| {
            Rc::new(boxed::factory(fn_service(
                |req: WebRequest<Err>| async move {
                    Ok(req.into_response(Response::NotFound().finish()))
                },
            )))
        });

        // App config
        let mut config =
            WebServiceConfig::new(config, default.clone(), self.data.clone());

        // register services
        std::mem::take(&mut *self.services.borrow_mut())
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
            case_insensitive: self.case_insensitive,
        });

        // external resources
        for mut rdef in std::mem::take(&mut *self.external.borrow_mut()) {
            rmap.add(&mut rdef, None);
        }

        // complete ResourceMap tree creation
        let rmap = Rc::new(rmap);
        rmap.finish(rmap.clone());

        let fut = self.endpoint.new_service(());
        let data = self.data.clone();
        let data_factories = self.data_factories.clone();
        let mut extensions = self
            .extensions
            .borrow_mut()
            .take()
            .unwrap_or_else(Extensions::new);

        Box::pin(async move {
            // main service
            let service = fut.await?;

            // create app data container
            for f in data.iter() {
                f.create(&mut extensions);
            }

            // async data factories
            for fut in data_factories.iter() {
                if let Ok(f) = fut().await {
                    f.create(&mut extensions);
                }
            }

            Ok(AppFactoryService {
                service,
                rmap,
                config,
                data: Rc::new(extensions),
                pool: HttpRequestPool::create(),
                _t: PhantomData,
            })
        })
    }
}

/// Service to convert `Request` to a `WebRequest<Err>`
pub struct AppFactoryService<T, Err>
where
    T: Service<
        Request = WebRequest<Err>,
        Response = WebResponse,
        Error = Err::Container,
    >,
    Err: ErrorRenderer,
{
    service: T,
    rmap: Rc<ResourceMap>,
    config: AppConfig,
    data: Rc<Extensions>,
    pool: &'static HttpRequestPool,
    _t: PhantomData<Err>,
}

impl<T, Err> Service for AppFactoryService<T, Err>
where
    T: Service<
        Request = WebRequest<Err>,
        Response = WebResponse,
        Error = Err::Container,
    >,
    Err: ErrorRenderer,
{
    type Request = Request;
    type Response = WebResponse;
    type Error = T::Error;
    type Future = T::Future;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    fn call(&self, req: Request) -> Self::Future {
        let (head, payload) = req.into_parts();

        let req = if let Some(mut req) = self.pool.get_request() {
            let inner = Rc::get_mut(&mut req.0).unwrap();
            inner.path.set(head.uri.clone());
            inner.head = head;
            inner.payload = payload;
            inner.app_data = self.data.clone();
            req
        } else {
            HttpRequest::new(
                Path::new(head.uri.clone()),
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

impl<T, Err> Drop for AppFactoryService<T, Err>
where
    T: Service<
        Request = WebRequest<Err>,
        Response = WebResponse,
        Error = Err::Container,
    >,
    Err: ErrorRenderer,
{
    fn drop(&mut self) {
        self.pool.clear();
    }
}

pub struct AppRoutingFactory<Err: ErrorRenderer> {
    services: Rc<Vec<(ResourceDef, HttpNewService<Err>, RefCell<Option<Guards>>)>>,
    default: Rc<HttpNewService<Err>>,
    case_insensitive: bool,
}

impl<Err: ErrorRenderer> ServiceFactory for AppRoutingFactory<Err> {
    type Config = ();
    type Request = WebRequest<Err>;
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = AppRouting<Err>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let services = self.services.clone();
        let default_fut = self.default.new_service(());

        let mut router = Router::build();
        if self.case_insensitive {
            router.case_insensitive();
        }

        Box::pin(async move {
            // create http services
            for (path, factory, guards) in &mut services.iter() {
                let service = factory.new_service(()).await?;
                router.rdef(path.clone(), service).2 = guards.borrow_mut().take();
            }

            Ok(AppRouting {
                ready: None,
                router: router.finish(),
                default: Some(default_fut.await?),
            })
        })
    }
}

pub struct AppRouting<Err: ErrorRenderer> {
    router: Router<HttpService<Err>, Guards>,
    ready: Option<(WebRequest<Err>, ResourceInfo)>,
    default: Option<HttpService<Err>>,
}

impl<Err: ErrorRenderer> Service for AppRouting<Err> {
    type Request = WebRequest<Err>;
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = BoxResponse<Err>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.ready.is_none() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&self, mut req: WebRequest<Err>) -> Self::Future {
        let res = self.router.recognize_checked(&mut req, |req, guards| {
            if let Some(guards) = guards {
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
        } else if let Some(ref default) = self.default {
            default.call(req)
        } else {
            let req = req.into_parts().0;
            Box::pin(async { Ok(WebResponse::new(Response::NotFound().finish(), req)) })
        }
    }
}

/// Wrapper service for routing
pub struct AppEntry<Err: ErrorRenderer> {
    factory: Rc<RefCell<Option<AppRoutingFactory<Err>>>>,
}

impl<Err: ErrorRenderer> AppEntry<Err> {
    pub fn new(factory: Rc<RefCell<Option<AppRoutingFactory<Err>>>>) -> Self {
        AppEntry { factory }
    }
}

impl<Err: ErrorRenderer> ServiceFactory for AppEntry<Err> {
    type Config = ();
    type Request = WebRequest<Err>;
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = AppRouting<Err>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

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

    #[crate::rt_test]
    async fn test_drop_data() {
        let data = Arc::new(AtomicBool::new(false));

        {
            let app =
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
