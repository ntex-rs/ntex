use std::task::{Context, Poll};
use std::{cell::RefCell, convert::Infallible, future::Future, pin::Pin, rc::Rc};

use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, Router};
use crate::service::{Service, ServiceFactory, Transform};
use crate::util::{ready, Either, Extensions};

use super::boxed::{BoxService, BoxServiceFactory};
use super::httprequest::{HttpRequest, HttpRequestPool};
use super::service::{WebServiceConfig, WebServiceWrapper};
use super::types::state::StateFactory;
use super::{config::AppConfig, guard::Guard, rmap::ResourceMap, stack::FiltersFactory};
use super::{ErrorRenderer, WebRequest, WebResponse};

type Guards = Vec<Box<dyn Guard>>;
type BoxResponse<'a> = Pin<Box<dyn Future<Output = Result<WebResponse, Infallible>> + 'a>>;
type FnStateFactory =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Box<dyn StateFactory>, ()>>>>>;

/// Service factory for converting `Request` to a `WebResponse>`.
///
/// It also executes state factories.
pub struct AppFactory(WebAppFactory);

pub struct AppService(WebAppHandler);

type WebAppFactory =
    Box<dyn Fn(AppConfig) -> Pin<Box<dyn Future<Output = Result<AppService, ()>>>>>;
type WebAppHandler =
    Box<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Result<WebResponse, Infallible>>>>>;
type WebAppHandler2<'a> = Box<
    dyn Fn(Request) -> Pin<Box<dyn Future<Output = Result<WebResponse, Infallible>> + 'a>>
        + 'a,
>;

impl AppFactory {
    pub(super) fn new<'a, M, F, Err: ErrorRenderer>(
        app: AppFactoryInner<'a, M, F, Err>,
    ) -> Self
    where
        M: Transform<
                AppRouting<
                    'a,
                    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service,
                    Err,
                >,
            > + 'static,
        M::Service: Service<
            &'a mut WebRequest<'a, Err>,
            Response = WebResponse,
            Error = Infallible,
        >,
        F: FiltersFactory<'a, Err> + 'static,
        <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service: 'static,
        <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Future: 'static,
        Err: ErrorRenderer,
    {
        let app = RefCell::new(app);
        let b: Box<
            dyn Fn(AppConfig) -> Pin<Box<dyn Future<Output = Result<AppService, ()>>>> + 'a,
        > = Box::new(move |cfg| {
            let fut = app.borrow_mut().create(cfg);
            Box::pin(async move { Ok(AppService(fut.await?)) })
        });
        AppFactory(unsafe { std::mem::transmute(b) })
    }
}

impl ServiceFactory<Request> for AppFactory {
    type Response = WebResponse;
    type Error = Infallible;
    type InitError = ();
    type Service = AppService;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ServiceFactory::<Request, AppConfig>::new_service(self, AppConfig::default())
    }
}

impl ServiceFactory<Request, AppConfig> for AppFactory {
    type Response = WebResponse;
    type Error = Infallible;
    type InitError = ();
    type Service = AppService;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, cfg: AppConfig) -> Self::Future {
        (&*self.0)(cfg)
    }
}

impl Service<Request> for AppService {
    type Response = WebResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, req: Request) -> Self::Future {
        (&*self.0)(req)
    }
}

/// Service factory to convert `Request` to a `WebRequest<S>`.
/// It also executes state factories.
pub(super) struct AppFactoryInner<'a, M, F, Err: ErrorRenderer>
where
    M: Transform<
        AppRouting<
            'a,
            <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service,
            Err,
        >,
    >,
    M::Service:
        Service<&'a mut WebRequest<'a, Err>, Response = WebResponse, Error = Infallible>,
    F: FiltersFactory<'a, Err>,
    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service: 'static,
    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Future: 'static,
    Err: ErrorRenderer,
{
    pub(super) middleware: Rc<M>,
    pub(super) filter: Option<F>,
    pub(super) extensions: RefCell<Option<Extensions>>,
    pub(super) state: Rc<Vec<Box<dyn StateFactory>>>,
    pub(super) state_factories: Rc<Vec<FnStateFactory>>,
    pub(super) services: Rc<RefCell<Vec<Box<dyn WebServiceWrapper<'a, Err>>>>>,
    pub(super) default: Option<BoxServiceFactory<'a, Err>>,
    pub(super) external: RefCell<Vec<ResourceDef>>,
    pub(super) case_insensitive: bool,
}

impl<'a, T, F, Err> AppFactoryInner<'a, T, F, Err>
where
    T: Transform<
            AppRouting<
                'a,
                <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service,
                Err,
            >,
        > + 'static,
    T::Service:
        Service<&'a mut WebRequest<'a, Err>, Response = WebResponse, Error = Infallible>,
    F: FiltersFactory<'a, Err> + 'static,
    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service: 'static,
    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Future: 'static,
    Err: ErrorRenderer + 'static,
{
    pub(super) fn create(
        &mut self,
        config: AppConfig,
    ) -> Pin<Box<dyn Future<Output = Result<WebAppHandler, ()>>>> {
        // update resource default service
        let default = Rc::new(self.default.take().unwrap());

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

        let filter_fut = self.filter.take().unwrap().create().new_service(());
        let state = self.state.clone();
        let state_factories = self.state_factories.clone();
        let mut extensions = self
            .extensions
            .borrow_mut()
            .take()
            .unwrap_or_else(Extensions::new);
        let middleware = self.middleware.clone();

        let f: Pin<Box<dyn Future<Output = Result<WebAppHandler, ()>> + 'a>> =
            Box::pin(async move {
                // create http services
                for (path, factory, guards) in &mut services.iter() {
                    let service = factory.new_service(()).await?;
                    router.rdef(path.clone(), service).2 = guards.borrow_mut().take();
                }

                // router
                let routing = AppRouting {
                    filter: filter_fut.await?,
                    router: Rc::new(AppRouter {
                        router: router.finish(),
                        default: Some(default_fut.await?),
                    }),
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

                let service = middleware.new_transform(routing);
                let state = Rc::new(extensions);
                let pool = HttpRequestPool::create();

                let hnd: WebAppHandler2<'a> = Box::new(move |req: Request| {
                    let (head, payload) = req.into_parts();

                    let http_req;
                    let mut web_req = if let Some(mut req) = pool.get_request() {
                        req.request.path.set(head.uri.clone());
                        req.request.head = head;
                        req.payload = payload;
                        req.request.app_state = state.clone();
                        let web_req = WebRequest::<Err>::new(unsafe {
                            std::mem::transmute(&mut *req)
                        });
                        http_req = Either::Left(req);
                        web_req
                    } else {
                        let mut req = HttpRequest::create(
                            Path::new(head.uri.clone()),
                            head,
                            payload,
                            rmap.clone(),
                            config.clone(),
                            state.clone(),
                        );
                        let web_req = WebRequest::<Err>::new(unsafe {
                            std::mem::transmute(&mut req)
                        });
                        http_req = Either::Right(req);
                        web_req
                    };
                    let fut = service.call(unsafe { std::mem::transmute(&mut web_req) });
                    Box::pin(async move {
                        let mut res = fut.await.unwrap();

                        let head = web_req.head();
                        if head.upgrade() {
                            res.response.head_mut().set_io(head);
                        }
                        drop(web_req);

                        match http_req {
                            Either::Left(req) => pool.release_boxed(req),
                            Either::Right(req) => pool.release_unboxed(req),
                        }
                        Ok(res)
                    })
                });
                Ok(unsafe { std::mem::transmute(hnd) })
            });
        unsafe { std::mem::transmute(f) }
    }
}

pub struct AppRouting<'a, F, Err: ErrorRenderer> {
    filter: F,
    router: Rc<AppRouter<'a, Err>>,
}

struct AppRouter<'a, Err: ErrorRenderer> {
    router: Router<BoxService<'a, Err>, Guards>,
    default: Option<BoxService<'a, Err>>,
}

impl<'a, F, Err: ErrorRenderer> Service<&'a mut WebRequest<'a, Err>>
    for AppRouting<'a, F, Err>
where
    F: Service<
        &'a mut WebRequest<'a, Err>,
        Response = &'a mut WebRequest<'a, Err>,
        Error = Infallible,
    >,
    F::Future: 'a,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Infallible;
    type Future = BoxResponse<'a>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = ready!(self.filter.poll_ready(cx));
        Poll::Ready(Ok(()))
    }

    fn call(&self, req: &'a mut WebRequest<'a, Err>) -> Self::Future {
        let fut = self.filter.call(req);
        let router = self.router.clone();

        Box::pin(async move {
            let req = fut.await.unwrap();

            let res = router.router.recognize_checked(req, |req, guards| {
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
                srv.call(req).await
            } else if let Some(ref default) = router.default {
                default.call(req).await
            } else {
                Ok(WebResponse::new(Response::NotFound().finish()))
            }
        })
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
