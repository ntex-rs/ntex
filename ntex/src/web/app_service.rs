use std::task::{Context, Poll};
use std::{cell::RefCell, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, Router};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::service::{fn_service, PipelineFactory, Service, ServiceFactory, Transform};
use crate::util::Extensions;

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
pub struct AppFactory<T, F, Err: ErrorRenderer>
where
    F: ServiceFactory<
        WebRequest<Err>,
        Response = WebRequest<Err>,
        Error = Err::Container,
        InitError = (),
    >,
    F::Future: 'static,
    Err: ErrorRenderer,
{
    pub(super) middleware: Rc<T>,
    pub(super) filter: PipelineFactory<F, WebRequest<Err>>,
    pub(super) extensions: RefCell<Option<Extensions>>,
    pub(super) data: Rc<Vec<Box<dyn DataFactory>>>,
    pub(super) data_factories: Rc<Vec<FnDataFactory>>,
    pub(super) services: Rc<RefCell<Vec<Box<dyn AppServiceFactory<Err>>>>>,
    pub(super) default: Option<Rc<HttpNewService<Err>>>,
    pub(super) external: RefCell<Vec<ResourceDef>>,
    pub(super) case_insensitive: bool,
}

impl<T, F, Err> ServiceFactory<Request, AppConfig> for AppFactory<T, F, Err>
where
    T: Transform<AppService<F::Service, Err>> + 'static,
    T::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
        WebRequest<Err>,
        Response = WebRequest<Err>,
        Error = Err::Container,
        InitError = (),
    >,
    F::Future: 'static,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
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
        let mut config = WebServiceConfig::new(config, default.clone(), self.data.clone());

        // register services
        std::mem::take(&mut *self.services.borrow_mut())
            .into_iter()
            .for_each(|mut srv| srv.register(&mut config));
        let (config, services) = config.into_services();

        // resource map
        let mut rmap = ResourceMap::new(ResourceDef::new(""));
        for mut rdef in std::mem::take(&mut *self.external.borrow_mut()) {
            rmap.add(&mut rdef, None);
        }

        // complete pipeline creation
        let services: Vec<_> = services
            .into_iter()
            .map(|(mut rdef, srv, guards, nested)| {
                rmap.add(&mut rdef, nested);
                (rdef, srv, RefCell::new(guards))
            })
            .collect();
        let default_fut = default.new_service(());

        let mut router = Router::build();
        if self.case_insensitive {
            router.case_insensitive();
        }

        // complete ResourceMap tree creation
        let rmap = Rc::new(rmap);
        rmap.finish(rmap.clone());

        let filter_fut = self.filter.new_service(());
        let data = self.data.clone();
        let data_factories = self.data_factories.clone();
        let mut extensions = self
            .extensions
            .borrow_mut()
            .take()
            .unwrap_or_else(Extensions::new);
        let middleware = self.middleware.clone();

        Box::pin(async move {
            // create http services
            for (path, factory, guards) in &mut services.iter() {
                let service = factory.new_service(()).await?;
                router.rdef(path.clone(), service).2 = guards.borrow_mut().take();
            }

            let routing = AppRouting {
                router: router.finish(),
                default: Some(default_fut.await?),
            };

            // main service
            let service = AppService {
                filter: filter_fut.await?,
                routing: Rc::new(routing),
            };

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
                rmap,
                config,
                service: middleware.new_transform(service),
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
    T: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    service: T,
    rmap: Rc<ResourceMap>,
    config: AppConfig,
    data: Rc<Extensions>,
    pool: &'static HttpRequestPool,
    _t: PhantomData<Err>,
}

impl<T, Err> Service<Request> for AppFactoryService<T, Err>
where
    T: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
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
    T: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    fn drop(&mut self) {
        self.pool.clear();
    }
}

struct AppRouting<Err: ErrorRenderer> {
    router: Router<HttpService<Err>, Guards>,
    default: Option<HttpService<Err>>,
}

impl<Err: ErrorRenderer> Service<WebRequest<Err>> for AppRouting<Err> {
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = BoxResponse<Err>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
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

/// Web app service
pub struct AppService<F, Err: ErrorRenderer> {
    filter: F,
    routing: Rc<AppRouting<Err>>,
}

impl<F, Err> Service<WebRequest<Err>> for AppService<F, Err>
where
    F: Service<WebRequest<Err>, Response = WebRequest<Err>, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = AppServiceResponse<F, Err>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready1 = self.filter.poll_ready(cx)?.is_ready();
        let ready2 = self.routing.poll_ready(cx)?.is_ready();
        if ready1 && ready2 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&self, req: WebRequest<Err>) -> Self::Future {
        AppServiceResponse {
            filter: self.filter.call(req),
            routing: self.routing.clone(),
            endpoint: None,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct AppServiceResponse<F: Service<WebRequest<Err>>, Err: ErrorRenderer> {
        #[pin]
        filter: F::Future,
        routing: Rc<AppRouting<Err>>,
        endpoint: Option<BoxResponse<Err>>,
    }
}

impl<F, Err> Future for AppServiceResponse<F, Err>
where
    F: Service<WebRequest<Err>, Response = WebRequest<Err>, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Err::Container>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        if let Some(fut) = this.endpoint.as_mut() {
            Pin::new(fut).poll(cx)
        } else {
            let res = if let Poll::Ready(res) = this.filter.poll(cx) {
                res?
            } else {
                return Poll::Pending;
            };
            *this.endpoint = Some(this.routing.call(res));
            self.poll(cx)
        }
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
            let app = init_service(
                App::new()
                    .data(DropData(data.clone()))
                    .service(web::resource("/test").to(|| async { HttpResponse::Ok() })),
            )
            .await;
            let req = TestRequest::with_uri("/test").to_request();
            let _ = app.call(req).await.unwrap();
        }
        assert!(data.load(Ordering::Relaxed));
    }
}
