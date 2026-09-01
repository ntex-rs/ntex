use std::{cell::RefCell, marker, mem, rc::Rc};

use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, ResourceId, Router};
use crate::service::cfg::{Cfg, Configuration};
use crate::service::{Ctx, Middleware, Service, ServiceFactory, factory};
use crate::service::{boxed, dev::ServiceChainFactory};
use crate::util::HashMap;

use super::config::WebAppConfig;
use super::error::AppInitError;
use super::guard::Guard;
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, WebServiceConfig};
use super::{AppState, HttpHandler, HttpRequest, HttpService, WebRequest, WebResponse};

type Guards = Vec<Box<dyn Guard>>;

/// Service factory to convert `Request` to a `WebRequest`.
/// It also executes state factories.
#[derive(derive_more::Debug)]
#[debug("AppFactory")]
pub struct AppFactory<St, Cfg, M, F>
where
    St: AppState,
    F: ServiceFactory<St, WebRequest, Cfg, Res = WebRequest, Error = St::Error, InitError = ()>,
{
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest, Cfg>,
    rmap: Rc<ResourceMap>,
    router: Rc<Router<HttpService<St, Cfg>, Guards>>,
    default: HttpService<St, Cfg>,
}

impl<St, Cfg, M, F> AppFactory<St, Cfg, M, F>
where
    St: AppState,
    Cfg: Clone + 'static,
    M: Middleware<AppRouter<St, Cfg, F::Service>, St, Cfg> + 'static,
    M::Service: Service<St, WebRequest, Res = WebResponse, Error = St::Error>,
    F: ServiceFactory<St, WebRequest, Cfg, Res = WebRequest, Error = St::Error, InitError = ()>,
{
    pub(super) fn new(
        middleware: M,
        filter: ServiceChainFactory<F, St, WebRequest, Cfg>,
        services: Vec<Box<dyn AppServiceFactory<St, Cfg>>>,
        default: Option<HttpService<St, Cfg>>,
        external: Vec<ResourceDef>,
        case_insensitive: bool,
    ) -> Self {
        // Default service
        let default = default.unwrap_or_else(|| {
            boxed::factory(
                factory(async move |req: WebRequest| {
                    Ok(req.into_response(Response::NotFound().finish()))
                })
                .map_init_err(|_| unreachable!()),
            )
        });

        // Web app config
        let mut config = WebServiceConfig::new(default);

        // register services
        for mut srv in services {
            srv.register(&mut config);
        }

        // ResourceMap tree
        let mut rmap = ResourceMap::new(ResourceDef::new(""));
        for mut rdef in external {
            rmap.add(&mut rdef, None);
        }

        // Complete pipeline creation
        let (services, default) = config.into_services();
        let services: Vec<_> = services
            .into_iter()
            .map(|(mut rdef, srv, guards, nested)| {
                rmap.add(&mut rdef, nested);
                (rdef, srv, RefCell::new(guards))
            })
            .collect();

        // complete ResourceMap tree
        let rmap = Rc::new(rmap);
        rmap.finish(&rmap);

        // Create router
        let mut router = Router::build();
        if case_insensitive {
            router.case_insensitive();
        }
        for (path, factory, guards) in services {
            router.rdef(path.clone(), factory).2 = guards.borrow_mut().take();
        }

        Self {
            rmap,
            filter,
            middleware,
            default,
            router: Rc::new(router.finish()),
        }
    }
}

impl<St, Cfg, M, F> ServiceFactory<St, Request, Cfg> for AppFactory<St, Cfg, M, F>
where
    St: AppState,
    Cfg: Clone + 'static,
    M: Middleware<AppRouter<St, Cfg, F::Service>, St, Cfg> + 'static,
    M::Service: Service<St, WebRequest, Res = WebResponse, Error = St::Error>,
    F: ServiceFactory<St, WebRequest, Cfg, Res = WebRequest, Error = St::Error, InitError = ()>,
{
    type Res = WebResponse;
    type Error = St::Error;

    type Service = AppService<M::Service, St>;
    type InitError = AppInitError;

    async fn create(&self, cfg: &Cfg) -> Result<Self::Service, Self::InitError> {
        let filter = self.filter.create(cfg).await.map_err(|e| {
            log::error!("Cannot construct app filter: {e:?}");
            AppInitError
        })?;

        // main service
        let service = self.middleware.create(
            AppRouter {
                filter,
                cfg: cfg.clone(),
                router: self.router.clone(),
                default: self.default.clone(),
                cache: RefCell::new(HashMap::default()),
                cache_default: RefCell::new(None),
            },
            cfg,
        );

        Ok(AppService {
            service,
            rmap: self.rmap.clone(),
            _t: marker::PhantomData,
        })
    }
}

/// Service to convert `Request` to a `WebRequest`
#[derive(derive_more::Debug)]
#[debug("AppService")]
pub struct AppService<S, St>
where
    S: Service<St, WebRequest, Res = WebResponse, Error = St::Error>,
    St: AppState,
{
    service: S,
    rmap: Rc<ResourceMap>,
    _t: marker::PhantomData<St>,
}

impl<S, St> Service<St, Request> for AppService<S, St>
where
    S: Service<St, WebRequest, Res = WebResponse, Error = St::Error>,
    St: AppState,
{
    type Res = WebResponse;
    type Error = S::Error;

    crate::forward_ready!(St, service);
    crate::forward_shutdown!(St, service);

    async fn call(&self, req: Request, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, S::Error> {
        let config: Cfg<WebAppConfig> = if let Some(io) = req.io() {
            io.cfg().ctx().get()
        } else {
            Cfg::<WebAppConfig>::default()
        };

        let (head, payload) = req.into_parts();

        let req = if let Some(mut req) = config.get_request() {
            let inner = Rc::get_mut(&mut req.0).unwrap();
            inner.path.set(head.uri.clone());
            inner.head = head;
            inner.config = config;
            req
        } else {
            HttpRequest::new(Path::new(head.uri.clone()), head, self.rmap.clone(), config)
        };
        ctx.call(&self.service, WebRequest::new(req, payload)).await
    }
}

/// Web app service.
#[derive(derive_more::Debug)]
#[debug("HttpRouter")]
pub struct AppRouter<St: AppState, Cfg, F> {
    pub(super) cfg: Cfg,
    pub(super) filter: F,
    pub(super) router: Rc<Router<HttpService<St, Cfg>, Guards>>,
    pub(super) default: HttpService<St, Cfg>,
    pub(super) cache: RefCell<HashMap<ResourceId, HttpHandler<St>>>,
    pub(super) cache_default: RefCell<Option<HttpHandler<St>>>,
}

impl<St, Cfg, F> Service<St, WebRequest> for AppRouter<St, Cfg, F>
where
    St: AppState,
    F: Service<St, WebRequest, Res = WebRequest, Error = St::Error>,
{
    type Res = WebResponse;
    type Error = St::Error;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.ready(&self.filter).await
    }

    async fn call(
        &self,
        req: WebRequest,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        let mut req = ctx.call(&self.filter, req).await?;
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

        let svc = if let Some((sf, id)) = res {
            if let Some(svc) = self.cache.borrow().get(&id) {
                svc.clone()
            } else if let Ok(svc) = sf.create(&self.cfg).await {
                self.cache.borrow_mut().insert(id, svc.clone());
                svc
            } else {
                return Ok(req.into_response(Response::InternalServerError().finish()));
            }
        } else {
            if let Some(svc) = &*self.cache_default.borrow() {
                svc.clone()
            } else if let Ok(svc) = self.default.create(&self.cfg).await {
                *self.cache_default.borrow_mut() = Some(svc.clone());
                svc
            } else {
                return Ok(req.into_response(Response::InternalServerError().finish()));
            }
        };
        ctx.call(&svc, req).await
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        ctx.shutdown(&self.filter).await;

        let svc = self.cache_default.borrow_mut().take();
        if let Some(svc) = svc {
            ctx.shutdown(&svc).await;
        }

        let services = mem::take(&mut *self.cache.borrow_mut());
        for (_, svc) in services {
            ctx.shutdown(&svc).await;
        }
    }
}
