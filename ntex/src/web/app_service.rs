use std::convert::Infallible;
use std::task::{Context, Poll};
use std::{cell::RefCell, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, Router};
use crate::service::{
    fn_service, into_service, PipelineFactory, Service, ServiceFactory, Transform,
};
use crate::util::{ready, Extensions};

use super::boxed::{self, BoxService, BoxServiceFactory};
use super::config::AppConfig;
use super::guard::Guard;
use super::httprequest::{HttpRequest, HttpRequestPool};
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, WebServiceConfig};
use super::stack::Next;
use super::types::state::StateFactory;
use super::{ErrorRenderer, WebRequest, WebResponse};

type Guards = Vec<Box<dyn Guard>>;
type BoxResponse<'a, Err: ErrorRenderer> =
    Pin<Box<dyn Future<Output = Result<WebResponse, Err::Container>> + 'a>>;
type FnStateFactory =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Box<dyn StateFactory>, ()>>>>>;

/// Service factory to convert `Request` to a `WebRequest<S>`.
/// It also executes state factories.
pub struct AppFactory<'a, T, F, Err: ErrorRenderer>
where
    F: ServiceFactory<
            &'a mut WebRequest<Err>,
            Response = &'a mut WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    Err: ErrorRenderer,
{
    pub(super) middleware: Rc<T>,
    pub(super) filter: PipelineFactory<F, &'a mut WebRequest<Err>>,
    pub(super) extensions: RefCell<Option<Extensions>>,
    pub(super) state: Rc<Vec<Box<dyn StateFactory>>>,
    pub(super) state_factories: Rc<Vec<FnStateFactory>>,
    pub(super) services: Rc<RefCell<Vec<Box<dyn AppServiceFactory<'a, Err>>>>>,
    pub(super) default: Option<Rc<BoxServiceFactory<'a, Err>>>,
    pub(super) external: RefCell<Vec<ResourceDef>>,
    pub(super) case_insensitive: bool,
}

impl<'a, T, F, Err> ServiceFactory<Request> for AppFactory<'a, T, F, Err>
where
    T: Transform<Next<AppService<'a, F::Service, Err>, Err>> + 'static,
    T::Service:
        Service<&'a mut WebRequest<Err>, Response = WebResponse, Error = Infallible>,
    F: ServiceFactory<
            &'a mut WebRequest<Err>,
            Response = &'a mut WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = AppFactoryService<'a, T::Service, Err>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>> + 'a>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ServiceFactory::<Request, AppConfig>::new_service(self, AppConfig::default())
    }
}

impl<'a, T, F, Err> ServiceFactory<Request, AppConfig> for AppFactory<'a, T, F, Err>
where
    T: Transform<Next<AppService<'a, F::Service, Err>, Err>> + 'static,
    T::Service:
        Service<&'a mut WebRequest<Err>, Response = WebResponse, Error = Infallible>,
    F: ServiceFactory<
            &'a mut WebRequest<Err>,
            Response = &'a mut WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = AppFactoryService<'a, T::Service, Err>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>> + 'a>>;

    fn new_service(&self, config: AppConfig) -> Self::Future {
        // update resource default service
        let default = self.default.clone().unwrap_or_else(|| panic!());

        //     .unwrap_or_else(|| {
        //     Rc::new(boxed::factory(into_service(
        //         |req: &'a mut WebRequest<Err>| {
        //             let res = req.into_response(Response::NotFound().finish());
        //             async move {
        //                 Ok(res)
        //             }
        //         }
        //     )))
        // });

        // App config
        let mut config = WebServiceConfig::new(config, default.clone(), self.state.clone());

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
        let state = self.state.clone();
        let state_factories = self.state_factories.clone();
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

            // create app state container
            for f in state.iter() {
                f.create(&mut extensions);
            }

            // async state factories
            for fut in state_factories.iter() {
                if let Ok(f) = fut().await {
                    f.create(&mut extensions);
                }
            }

            Ok(AppFactoryService {
                rmap,
                config,
                service: middleware.new_transform(Next::new(service)),
                state: Rc::new(extensions),
                pool: HttpRequestPool::create(),
                _t: PhantomData,
            })
        })
    }
}

/// Service to convert `Request` to a `WebRequest<Err>`
pub struct AppFactoryService<'a, T, Err>
where
    T: Service<&'a mut WebRequest<Err>, Response = WebResponse, Error = Infallible>,
    Err: ErrorRenderer,
{
    service: T,
    rmap: Rc<ResourceMap>,
    config: AppConfig,
    state: Rc<Extensions>,
    pool: &'static HttpRequestPool,
    _t: PhantomData<&'a Err>,
}

impl<'a, T, Err> Service<Request> for AppFactoryService<'a, T, Err>
where
    T: Service<&'a mut WebRequest<Err>, Response = WebResponse, Error = Infallible>,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = AppFactoryServiceResponse<'a, T, Err>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = ready!(self.service.poll_ready(cx));
        Poll::Ready(Ok(()))
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
            inner.app_state = self.state.clone();
            req
        } else {
            HttpRequest::new(
                Path::new(head.uri.clone()),
                head,
                payload,
                self.rmap.clone(),
                self.config.clone(),
                self.state.clone(),
                self.pool,
            )
        };
        let mut req = WebRequest::new(req);
        let fut = self.service.call(&mut req);

        AppFactoryServiceResponse { fut, req }
    }
}

impl<'a, T, Err> Drop for AppFactoryService<'a, T, Err>
where
    T: Service<&'a mut WebRequest<Err>, Response = WebResponse, Error = Infallible>,
    Err: ErrorRenderer,
{
    fn drop(&mut self) {
        self.pool.clear();
    }
}

pin_project_lite::pin_project! {
    pub struct AppFactoryServiceResponse<'a, T: Service<&'a mut WebRequest<Err>>, Err>{
        #[pin]
        fut: T::Future,
        req: WebRequest<Err>,
    }
}

impl<'a, T, Err> Future for AppFactoryServiceResponse<'a, T, Err>
where
    T: Service<&'a mut WebRequest<Err>, Response = WebResponse, Error = Infallible>,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Err::Container>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(ready!(self.project().fut.poll(cx)).unwrap()))
    }
}

struct AppRouting<'a, Err: ErrorRenderer> {
    router: Router<BoxService<'a, Err>, Guards>,
    default: Option<BoxService<'a, Err>>,
}

impl<'a, Err: ErrorRenderer> Service<&'a mut WebRequest<Err>> for AppRouting<'a, Err> {
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = BoxResponse<'a, Err>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, mut req: &'a mut WebRequest<Err>) -> Self::Future {
        let res = self.router.recognize_checked(req, |req, guards| {
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
            Box::pin(async { Ok(WebResponse::new(Response::NotFound().finish())) })
        }
    }
}

/// Web app service
pub struct AppService<'a, F, Err: ErrorRenderer> {
    filter: F,
    routing: Rc<AppRouting<'a, Err>>,
}

impl<'a, F, Err> Service<&'a mut WebRequest<Err>> for AppService<'a, F, Err>
where
    F: Service<
        &'a mut WebRequest<Err>,
        Response = &'a mut WebRequest<Err>,
        Error = Err::Container,
    >,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = AppServiceResponse<'a, F, Err>;

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

    fn call(&self, req: &'a mut WebRequest<Err>) -> Self::Future {
        AppServiceResponse {
            filter: self.filter.call(req),
            routing: self.routing.clone(),
            endpoint: None,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct AppServiceResponse<'a, F: Service<&'a mut WebRequest<Err>>, Err: ErrorRenderer> {
        #[pin]
        filter: F::Future,
        routing: Rc<AppRouting<'a, Err>>,
        endpoint: Option<BoxResponse<'a, Err>>,
    }
}

impl<'a, F, Err> Future for AppServiceResponse<'a, F, Err>
where
    F: Service<
        &'a mut WebRequest<Err>,
        Response = &'a mut WebRequest<Err>,
        Error = Err::Container,
    >,
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
