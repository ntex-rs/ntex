use std::{cell::RefCell, marker, rc::Rc};

use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, Router};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::service::cfg::SharedCfg;
use crate::service::dev::ServiceChainFactory;
use crate::service::{Ctx, Middleware, ReadyCtx, Service, ServiceFactory, factory};
use crate::util::{BoxFuture, Extensions, join};

use super::error::{AppInitError, ErrorRenderer};
use super::guard::Guard;
use super::httprequest::HttpRequest;
use super::request::WebRequest;
use super::response::WebResponse;
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, AppState, WebServiceConfig};

type Guards = Vec<Box<dyn Guard>>;
type HttpService<Err: ErrorRenderer> = BoxService<(), WebRequest<Err>, WebResponse, Err::Container>;
type HttpNewService<Err: ErrorRenderer> =
    BoxServiceFactory<(), WebRequest<Err>, WebResponse, Err::Container, SharedCfg, ()>;
type FnStateFactory = Box<dyn Fn(Extensions) -> BoxFuture<'static, Result<Extensions, ()>>>;

/// Service factory to convert `Request` to a `WebRequest<S>`.
/// It also executes state factories.
#[derive(derive_more::Debug)]
#[debug("AppFactory")]
pub struct AppFactory<T, F, Err: ErrorRenderer>
where
    F: ServiceFactory<
            WebRequest<Err>,
            Res = WebRequest<Err>,
            Error = Err::Container,
            InitCfg = SharedCfg,
            InitError = (),
        >,
    Err: ErrorRenderer,
{
    pub(super) middleware: Rc<T>,
    pub(super) filter: ServiceChainFactory<F, (), WebRequest<Err>>,
    pub(super) extensions: RefCell<Option<Extensions>>,
    pub(super) state_factories: Rc<Vec<FnStateFactory>>,
    pub(super) services: Rc<RefCell<Vec<Box<dyn AppServiceFactory<Err>>>>>,
    pub(super) default: Option<Rc<HttpNewService<Err>>>,
    pub(super) external: RefCell<Vec<ResourceDef>>,
    pub(super) case_insensitive: bool,
}

impl<T, F, Err> ServiceFactory<Request> for AppFactory<T, F, Err>
where
    T: Middleware<AppService<F::Service, Err>, SharedCfg> + 'static,
    T::Service: Service<Req = WebRequest<Err>, Res = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
            WebRequest<Err>,
            Res = WebRequest<Err>,
            Error = Err::Container,
            InitCfg = SharedCfg,
            InitError = (),
        >,
    Err: ErrorRenderer,
{
    type Res = WebResponse;
    type Error = Err::Container;

    type Service = AppFactoryService<T::Service, Err>;
    type InitCfg = SharedCfg;
    type InitError = AppInitError;

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        let services = std::mem::take(&mut *self.services.borrow_mut());

        // update resource default service
        let default = self.default.clone().unwrap_or_else(|| {
            Rc::new(boxed::factory(
                factory(async move |req: WebRequest<Err>| {
                    Ok(req.into_response(Response::NotFound().finish()))
                })
                .map_init_err(|_| unreachable!()),
            ))
        });

        let filter_fut = self.filter.create(cfg);
        let state_factories = self.state_factories.clone();
        let mut extensions = self.extensions.borrow_mut().take().unwrap_or_default();
        let middleware = self.middleware.clone();
        let external = std::mem::take(&mut *self.external.borrow_mut());

        let mut router = Router::build();
        if self.case_insensitive {
            router.case_insensitive();
        }

        // app state factories
        for fut in state_factories.iter() {
            extensions = fut(extensions).await.map_err(|e| {
                log::error!("Cannot initialize state factory, {e:?}");
                AppInitError
            })?;
        }
        let state = AppState::new(extensions, None, cfg.get());

        // App config
        let mut config = WebServiceConfig::new(state.clone(), default.clone());

        // register services
        for mut srv in services {
            srv.register(&mut config);
        }
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
        rmap.finish(&rmap);

        // create http services
        for (path, factory, guards) in &mut services.iter() {
            let service = factory.create(cfg).await.map_err(|()| {
                log::error!("Cannot construct app service");
                AppInitError
            })?;
            router.rdef(path.clone(), service).2 = guards.borrow_mut().take();
        }

        let routing = AppRouting {
            router: router.finish(),
            default: Some(default.create(cfg).await.map_err(|e| {
                log::error!("Cannot construct default service, {e:?}");
                AppInitError
            })?),
        };

        // main service
        let service = AppService {
            routing,
            filter: filter_fut.await.map_err(|e| {
                log::error!("Cannot construct app filter: {e:?}");
                AppInitError
            })?,
        };

        Ok(AppFactoryService {
            rmap,
            state,
            service: middleware.create(service, cfg),
            _t: marker::PhantomData,
        })
    }
}

/// Service to convert `Request` to a `WebRequest<Err>`
#[derive(derive_more::Debug)]
#[debug("AppFactoryService")]
pub struct AppFactoryService<T, Err>
where
    T: Service<Req = WebRequest<Err>, Res = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    service: T,
    rmap: Rc<ResourceMap>,
    state: AppState,
    _t: marker::PhantomData<Err>,
}

impl<S, Err> Service<()> for AppFactoryService<S, Err>
where
    S: Service<Req = WebRequest<Err>, Res = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Req = Request;
    type Res = WebResponse;
    type Error = S::Error;

    crate::forward_ready!((), service);
    crate::forward_shutdown!(service);

    async fn call(&self, req: Request, ctx: Ctx<'_, Self, ()>) -> Result<Self::Res, S::Error> {
        let (head, payload) = req.into_parts();

        let req = if let Some(mut req) = self.state.config().get_request() {
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
            )
        };
        ctx.call(&self.service, WebRequest::new(req)).await
    }
}

impl<T, Err> Drop for AppFactoryService<T, Err>
where
    T: Service<Req = WebRequest<Err>, Res = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    fn drop(&mut self) {
        self.state.config().clear_requests();
    }
}

struct AppRouting<Err: ErrorRenderer> {
    router: Router<HttpService<Err>, Guards>,
    default: Option<HttpService<Err>>,
}

impl<Err: ErrorRenderer> Service for AppRouting<Err> {
    type Req = WebRequest<Err>;
    type Res = WebResponse;
    type Error = Err::Container;

    async fn call(
        &self,
        mut req: WebRequest<Err>,
        ctx: Ctx<'_, Self, ()>,
    ) -> Result<WebResponse, Err::Container> {
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
            ctx.call(srv, req).await
        } else if let Some(ref default) = self.default {
            ctx.call(default, req).await
        } else {
            let req = req.into_parts().0;
            Ok(WebResponse::new(Response::NotFound().finish(), req))
        }
    }
}

/// Web app service.
#[derive(derive_more::Debug)]
#[debug("AppService")]
pub struct AppService<F, Err: ErrorRenderer> {
    filter: F,
    routing: AppRouting<Err>,
}

impl<F, Err> Service for AppService<F, Err>
where
    F: Service<Req = WebRequest<Err>, Res = WebRequest<Err>, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Req = WebRequest<Err>;
    type Res = WebResponse;
    type Error = Err::Container;

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self, ()>) -> Result<(), Self::Error> {
        let (ready1, ready2) = join(ctx.ready(&self.filter), ctx.ready(&self.routing)).await;
        ready1?;
        ready2
    }

    #[inline]
    async fn call(
        &self,
        req: WebRequest<Err>,
        ctx: Ctx<'_, Self, ()>,
    ) -> Result<Self::Res, Self::Error> {
        let req = ctx.call(&self.filter, req).await?;
        ctx.call(&self.routing, req).await
    }

    async fn shutdown(&self) {
        self.filter.shutdown().await;
        self.routing.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::web::test::{TestRequest, init_service};
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
                    .service(web::resource("/test").to(async || HttpResponse::Ok())),
            )
            .await;
            let req = TestRequest::with_uri("/test").to_request();
            let _ = app.call(req).await.unwrap();
        }
        assert!(data.load(Ordering::Relaxed));
    }
}
