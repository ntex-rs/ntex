use std::task::{Context, Poll};
use std::{cell::RefCell, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, Router};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::service::dev::ServiceChainFactory;
use crate::service::{
    fn_service, Middleware, Service, ServiceCall, ServiceCtx, ServiceFactory,
};
use crate::util::{BoxFuture, Either, Extensions};

use super::config::AppConfig;
use super::error::ErrorRenderer;
use super::guard::Guard;
use super::httprequest::{HttpRequest, HttpRequestPool};
use super::request::WebRequest;
use super::response::WebResponse;
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, AppState, WebServiceConfig};

type Guards = Vec<Box<dyn Guard>>;
type HttpService<Err: ErrorRenderer> =
    BoxService<WebRequest<Err>, WebResponse, Err::Container>;
type HttpNewService<Err: ErrorRenderer> =
    BoxServiceFactory<(), WebRequest<Err>, WebResponse, Err::Container, ()>;
type BoxResponse<'a, Err: ErrorRenderer> =
    ServiceCall<'a, HttpService<Err>, WebRequest<Err>>;
type FnStateFactory = Box<dyn Fn(Extensions) -> BoxFuture<'static, Result<Extensions, ()>>>;

/// Service factory to convert `Request` to a `WebRequest<S>`.
/// It also executes state factories.
pub struct AppFactory<T, F, Err: ErrorRenderer>
where
    F: ServiceFactory<
        WebRequest<Err>,
        Response = WebRequest<Err>,
        Error = Err::Container,
        InitError = (),
    >,
    Err: ErrorRenderer,
{
    pub(super) middleware: Rc<T>,
    pub(super) filter: ServiceChainFactory<F, WebRequest<Err>>,
    pub(super) extensions: RefCell<Option<Extensions>>,
    pub(super) state_factories: Rc<Vec<FnStateFactory>>,
    pub(super) services: Rc<RefCell<Vec<Box<dyn AppServiceFactory<Err>>>>>,
    pub(super) default: Option<Rc<HttpNewService<Err>>>,
    pub(super) external: RefCell<Vec<ResourceDef>>,
    pub(super) case_insensitive: bool,
}

impl<T, F, Err> ServiceFactory<Request> for AppFactory<T, F, Err>
where
    T: Middleware<AppService<F::Service, Err>> + 'static,
    T::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
        WebRequest<Err>,
        Response = WebRequest<Err>,
        Error = Err::Container,
        InitError = (),
    >,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = AppFactoryService<T::Service, Err>;
    type Future<'f> = BoxFuture<'f, Result<Self::Service, Self::InitError>> where Self: 'f;

    fn create(&self, _: ()) -> Self::Future<'_> {
        ServiceFactory::create(self, AppConfig::default())
    }
}

impl<T, F, Err> ServiceFactory<Request, AppConfig> for AppFactory<T, F, Err>
where
    T: Middleware<AppService<F::Service, Err>> + 'static,
    T::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
        WebRequest<Err>,
        Response = WebRequest<Err>,
        Error = Err::Container,
        InitError = (),
    >,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = AppFactoryService<T::Service, Err>;
    type Future<'f> = BoxFuture<'f, Result<Self::Service, Self::InitError>> where Self: 'f;

    fn create(&self, config: AppConfig) -> Self::Future<'_> {
        let services = std::mem::take(&mut *self.services.borrow_mut());

        // update resource default service
        let default = self.default.clone().unwrap_or_else(|| {
            Rc::new(boxed::factory(fn_service(
                |req: WebRequest<Err>| async move {
                    Ok(req.into_response(Response::NotFound().finish()))
                },
            )))
        });

        let filter_fut = self.filter.create(());
        let state_factories = self.state_factories.clone();
        let mut extensions = self.extensions.borrow_mut().take().unwrap_or_default();
        let middleware = self.middleware.clone();
        let external = std::mem::take(&mut *self.external.borrow_mut());

        let mut router = Router::build();
        if self.case_insensitive {
            router.case_insensitive();
        }

        Box::pin(async move {
            // app state factories
            for fut in state_factories.iter() {
                extensions = fut(extensions).await?;
            }
            let state = AppState::new(extensions, None, config.clone());

            // App config
            let mut config = WebServiceConfig::new(state.clone(), default.clone());

            // register services
            services
                .into_iter()
                .for_each(|mut srv| srv.register(&mut config));
            let services = config.into_services();

            // resource map
            let mut rmap = ResourceMap::new(ResourceDef::new(""));
            for mut rdef in external {
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

            // complete ResourceMap tree creation
            let rmap = Rc::new(rmap);
            rmap.finish(rmap.clone());

            // create http services
            for (path, factory, guards) in &mut services.iter() {
                let service = factory.create(()).await?;
                router.rdef(path.clone(), service).2 = guards.borrow_mut().take();
            }

            let routing = AppRouting {
                router: router.finish(),
                default: Some(default.create(()).await?),
            };

            // main service
            let service = AppService {
                routing,
                filter: filter_fut.await?,
            };

            Ok(AppFactoryService {
                rmap,
                state,
                service: middleware.create(service),
                pool: HttpRequestPool::create(),
                _t: PhantomData,
            })
        })
    }
}

/// Service to convert `Request` to a `WebRequest<Err>`
#[derive(Debug)]
pub struct AppFactoryService<T, Err>
where
    T: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    service: T,
    rmap: Rc<ResourceMap>,
    state: AppState,
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
    type Future<'f> = ServiceCall<'f, T, WebRequest<Err>> where T: 'f;

    crate::forward_poll_ready!(service);
    crate::forward_poll_shutdown!(service);

    fn call<'a>(&'a self, req: Request, ctx: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        let (head, payload) = req.into_parts();

        let req = if let Some(mut req) = self.pool.get_request() {
            let inner = Rc::get_mut(&mut req.0).unwrap();
            inner.path.set(head.uri.clone());
            inner.head = head;
            inner.payload = payload;
            inner.app_state = self.state.clone();
            req
        } else {
            HttpRequest::new(
                Path::new(head.uri.clone()),
                head,
                payload,
                self.rmap.clone(),
                self.state.clone(),
                self.pool,
            )
        };
        ctx.call(&self.service, WebRequest::new(req))
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
    type Future<'f> =
        Either<BoxResponse<'f, Err>, BoxFuture<'f, Result<WebResponse, Err::Container>>>;

    fn call<'a>(
        &'a self,
        mut req: WebRequest<Err>,
        ctx: ServiceCtx<'a, Self>,
    ) -> Self::Future<'a> {
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
            Either::Left(ctx.call(srv, req))
        } else if let Some(ref default) = self.default {
            Either::Left(ctx.call(default, req))
        } else {
            let req = req.into_parts().0;
            Either::Right(Box::pin(async {
                Ok(WebResponse::new(Response::NotFound().finish(), req))
            }))
        }
    }
}

/// Web app service
pub struct AppService<F, Err: ErrorRenderer> {
    filter: F,
    routing: AppRouting<Err>,
}

impl<F, Err> Service<WebRequest<Err>> for AppService<F, Err>
where
    F: Service<WebRequest<Err>, Response = WebRequest<Err>, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type Future<'f> = AppServiceResponse<'f, F, Err> where F: 'f;

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

    fn call<'a>(
        &'a self,
        req: WebRequest<Err>,
        ctx: ServiceCtx<'a, Self>,
    ) -> Self::Future<'a> {
        AppServiceResponse {
            filter: ctx.call(&self.filter, req),
            routing: &self.routing,
            endpoint: None,
            ctx,
        }
    }
}

type BoxAppServiceResponse<'a, Err: ErrorRenderer> =
    ServiceCall<'a, AppRouting<Err>, WebRequest<Err>>;

pin_project_lite::pin_project! {
    pub struct AppServiceResponse<'f, F: Service<WebRequest<Err>>, Err: ErrorRenderer>
    where F: 'f
    {
        #[pin]
        filter: ServiceCall<'f, F, WebRequest<Err>>,
        routing: &'f AppRouting<Err>,
        endpoint: Option<BoxAppServiceResponse<'f, Err>>,
        ctx: ServiceCtx<'f, AppService<F, Err>>,
    }
}

impl<'f, F, Err> Future for AppServiceResponse<'f, F, Err>
where
    F: Service<WebRequest<Err>, Response = WebRequest<Err>, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Err::Container>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        loop {
            if let Some(fut) = this.endpoint.as_mut() {
                return Pin::new(fut).poll(cx);
            } else {
                let res = if let Poll::Ready(res) = this.filter.poll(cx) {
                    res?
                } else {
                    return Poll::Pending;
                };
                *this.endpoint = Some(this.ctx.call(this.routing, res));
                this = self.as_mut().project();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

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
                    .state(DropData(data.clone()))
                    .service(web::resource("/test").to(|| async { HttpResponse::Ok() })),
            )
            .await;
            let req = TestRequest::with_uri("/test").to_request();
            let _ = app.call(req).await.unwrap();
        }
        assert!(data.load(Ordering::Relaxed));
    }
}
