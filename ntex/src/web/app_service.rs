use std::{cell::RefCell, marker, mem, rc::Rc};

use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, ResourceId, Router};
use crate::service::cfg::{Cfg, SharedCfg};
use crate::service::{Ctx, Middleware, Service, ServiceFactory, factory_with_st};
use crate::service::{boxed, dev::ServiceChainFactory};
use crate::util::HashMap;

use super::config::WebAppConfig;
use super::error::{AppInitError, ErrorRenderer};
use super::guard::Guard;
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, WebServiceConfig};
use super::{HttpHandler, HttpService};
use super::{httprequest::HttpRequest, request::WebRequest, response::WebResponse};

type Guards = Vec<Box<dyn Guard>>;

/// Service factory to convert `Request` to a `WebRequest<S>`.
/// It also executes state factories.
#[derive(derive_more::Debug)]
#[debug("AppFactory")]
pub struct AppFactory<St, M, F, Err: ErrorRenderer>
where
    F: ServiceFactory<
            St,
            WebRequest<Err>,
            SharedCfg,
            Res = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    Err: ErrorRenderer,
{
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<Err>, SharedCfg>,
    rmap: Rc<ResourceMap>,
    router: Rc<Router<HttpService<St, Err>, Guards>>,
    default: Rc<HttpService<St, Err>>,
}

impl<St, M, F, Err> AppFactory<St, M, F, Err>
where
    St: 'static,
    M: Middleware<AppRouter<St, F::Service, Err>, SharedCfg> + 'static,
    M::Service: Service<St, Req = WebRequest<Err>, Res = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
            St,
            WebRequest<Err>,
            SharedCfg,
            Res = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    Err: ErrorRenderer,
{
    pub(super) fn new(
        middleware: M,
        filter: ServiceChainFactory<F, St, WebRequest<Err>, SharedCfg>,
        services: Vec<Box<dyn AppServiceFactory<St, Err>>>,
        default: Option<HttpService<St, Err>>,
        external: Vec<ResourceDef>,
        case_insensitive: bool,
    ) -> Self {
        // Default service
        let default = Rc::new(default.unwrap_or_else(|| {
            boxed::factory(
                factory_with_st(async move |req: WebRequest<Err>| {
                    Ok(req.into_response(Response::NotFound().finish()))
                })
                .map_init_err(|_| unreachable!()),
            )
        }));

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

impl<St, M, F, Err> ServiceFactory<St, Request, SharedCfg> for AppFactory<St, M, F, Err>
where
    St: 'static,
    M: Middleware<AppRouter<St, F::Service, Err>, SharedCfg> + 'static,
    M::Service: Service<St, Req = WebRequest<Err>, Res = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
            St,
            WebRequest<Err>,
            SharedCfg,
            Res = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    Err: ErrorRenderer,
{
    type Res = WebResponse;
    type Error = Err::Container;

    type Service = AppService<M::Service, St, Err>;
    type InitError = AppInitError;

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
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
            config: cfg.get(),
            rmap: self.rmap.clone(),
            _t: marker::PhantomData,
        })
    }
}

/// Service to convert `Request` to a `WebRequest<Err>`
#[derive(derive_more::Debug)]
#[debug("AppService")]
pub struct AppService<S, St, Err>
where
    S: Service<St, Req = WebRequest<Err>, Res = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    service: S,
    rmap: Rc<ResourceMap>,
    config: Cfg<WebAppConfig>,
    _t: marker::PhantomData<(St, Err)>,
}

impl<S, St, Err> Service<St> for AppService<S, St, Err>
where
    S: Service<St, Req = WebRequest<Err>, Res = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Req = Request;
    type Res = WebResponse;
    type Error = S::Error;

    crate::forward_ready!(St, service);
    crate::forward_shutdown!(service);

    async fn call(&self, req: Request, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, S::Error> {
        let (head, payload) = req.into_parts();

        let req = if let Some(mut req) = self.config.get_request() {
            let inner = Rc::get_mut(&mut req.0).unwrap();
            inner.path.set(head.uri.clone());
            inner.head = head;
            inner.payload = payload;
            inner.config = self.config.clone();
            req
        } else {
            HttpRequest::new(
                Path::new(head.uri.clone()),
                head,
                payload,
                self.rmap.clone(),
                self.config.clone(),
            )
        };
        ctx.call(&self.service, WebRequest::new(req)).await
    }
}

/// Web app service.
#[derive(derive_more::Debug)]
#[debug("HttpRouter")]
pub struct AppRouter<St, F, Err: ErrorRenderer> {
    pub(super) cfg: SharedCfg,
    pub(super) filter: F,
    pub(super) router: Rc<Router<HttpService<St, Err>, Guards>>,
    pub(super) default: Rc<HttpService<St, Err>>,
    pub(super) cache: RefCell<HashMap<ResourceId, HttpHandler<St, Err>>>,
    pub(super) cache_default: RefCell<Option<HttpHandler<St, Err>>>,
}

impl<St, F, Err> Service<St> for AppRouter<St, F, Err>
where
    St: 'static,
    F: Service<St, Req = WebRequest<Err>, Res = WebRequest<Err>, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Req = WebRequest<Err>;
    type Res = WebResponse;
    type Error = Err::Container;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.ready(&self.filter).await
    }

    async fn call(&self, req: Self::Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
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

    async fn shutdown(&self) {
        self.filter.shutdown().await;

        let svc = self.cache_default.borrow_mut().take();
        if let Some(svc) = svc {
            svc.shutdown().await;
        }

        let services = mem::take(&mut *self.cache.borrow_mut());
        for (_, svc) in services {
            svc.shutdown().await;
        }
    }
}
