use std::{cell::RefCell, marker::PhantomData, mem, rc::Rc};

use crate::error::Failure;
use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, ResourceId, Router};
use crate::service::{ServiceChainFactory, boxed, cfg::Cfg, cfg::Configuration};
use crate::util::HashMap;
use crate::{Ctx, Middleware, Service, ServiceFactory, factory};

use super::config::WebAppConfig;
use super::guard::Guard;
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, WebServiceConfig};
use super::{AppState, HttpHandler, HttpRequest, HttpService, WebError, WebRequest, WebResponse};

type Guards = Vec<Box<dyn Guard>>;

/// Service factory to convert `Request` to a `WebRequest`.
/// It also executes state factories.
#[derive(derive_more::Debug)]
#[debug("AppFactory")]
pub struct AppFactory<St, In, Out, M, F>
where
    St: AppState,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<AppRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    rmap: Rc<ResourceMap>,
    router: Rc<Router<HttpService<St, Out>, Guards>>,
    default: HttpService<St, Out>,
    config: Option<Cfg<WebAppConfig>>,
}

impl<St, In, Out, M, F> AppFactory<St, In, Out, M, F>
where
    St: AppState,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<AppRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    pub(super) fn new(
        middleware: M,
        filter: ServiceChainFactory<F, St, WebRequest<In>>,
        services: Vec<Box<dyn AppServiceFactory<St, Out>>>,
        default: Option<HttpService<St, Out>>,
        config: Option<Cfg<WebAppConfig>>,
        external: Vec<ResourceDef>,
        case_insensitive: bool,
    ) -> Self {
        // Default service
        let default = default.unwrap_or_else(|| {
            boxed::factory(
                factory(async move |req: WebRequest<Out>| {
                    Ok(req.into_response(Response::NotFound().build()))
                })
                .map_init_err(|_| unreachable!()),
            )
        });

        // Web app config
        let mut cfg = WebServiceConfig::new();

        // register services
        for mut srv in services {
            srv.register(&mut cfg);
        }

        // ResourceMap tree
        let mut rmap = ResourceMap::new(ResourceDef::new(""));
        for mut rdef in external {
            rmap.add(&mut rdef, None);
        }

        // Complete pipeline creation
        let services = cfg.into_services();
        let services: Vec<_> = services
            .into_iter()
            .map(|(mut rdef, srv, guards, nested)| {
                rmap.add(&mut rdef, nested);
                (rdef, srv, RefCell::new(guards))
            })
            .collect();

        // complete ResourceMap tree
        let rmap = Rc::new(rmap);
        rmap.build(&rmap);

        // Create router
        let mut router = Router::builder();
        if case_insensitive {
            router.case_insensitive();
        }
        for (path, factory, guards) in services {
            router.rdef(path.clone(), factory).2 = guards.borrow_mut().take();
        }

        Self {
            rmap,
            filter,
            default,
            config,
            middleware,
            router: Rc::new(router.build()),
        }
    }
}

impl<St, In, Out, M, F> ServiceFactory<St, Request> for AppFactory<St, In, Out, M, F>
where
    St: AppState,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<AppRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    type Res = Response;
    type Error = WebError<St, St::Error>;

    type Service = AppService<St, M::Service>;
    type InitError = Failure;

    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        // main service
        let service = AppRouter {
            filter: self.filter.create(st).await?,
            router: self.router.clone(),
            default: self.default.clone(),
            cache: RefCell::new(HashMap::default()),
            cache_default: RefCell::new(None),
            ph: PhantomData,
        };

        Ok(AppService {
            service: self.middleware.create(st, service),
            rmap: self.rmap.clone(),
            config: self.config.clone(),
            _t: PhantomData,
        })
    }
}

/// Service to convert `Request` to a `WebRequest`
#[derive(derive_more::Debug)]
#[debug("AppService")]
pub struct AppService<St, F>
where
    St: AppState,
    F: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    service: F,
    rmap: Rc<ResourceMap>,
    config: Option<Cfg<WebAppConfig>>,
    _t: PhantomData<St>,
}

impl<St, F> Service<St, Request> for AppService<St, F>
where
    St: AppState,
    F: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    type Res = Response;
    type Error = F::Error;

    crate::forward_ready!(St, service);
    crate::forward_shutdown!(St, service);

    async fn call(&self, req: Request, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, F::Error> {
        let config: Cfg<WebAppConfig> = if let Some(cfg) = &self.config {
            cfg.clone()
        } else if let Some(io) = req.io() {
            io.cfg().ctx().get()
        } else {
            Cfg::<WebAppConfig>::default()
        };

        let (head, payload) = req.into_parts();

        let req = if let Some(mut req) = config.get_request() {
            let inner = Rc::get_mut(&mut req.0).unwrap();
            inner.path.set(head.uri.clone());
            inner.head = head;
            req
        } else {
            HttpRequest::new(Path::new(head.uri.clone()), head, self.rmap.clone(), config)
        };
        match ctx
            .call(&self.service, WebRequest::new(req, payload, ()))
            .await
        {
            Ok(r) => Ok(r.into()),
            Err(mut e) => Ok(e.0.error_response(ctx.st())),
        }
    }
}

/// Web app service.
#[derive(derive_more::Debug)]
#[debug("AppRouter")]
pub struct AppRouter<St: AppState, In, Out, F> {
    pub(super) filter: F,
    pub(super) router: Rc<Router<HttpService<St, Out>, Guards>>,
    pub(super) default: HttpService<St, Out>,
    pub(super) cache: RefCell<HashMap<ResourceId, HttpHandler<St, Out>>>,
    pub(super) cache_default: RefCell<Option<HttpHandler<St, Out>>>,
    pub(super) ph: PhantomData<In>,
}

impl<St, In, Out, F> Service<St, WebRequest<In>> for AppRouter<St, In, Out, F>
where
    St: AppState,
    Out: 'static,
    F: Service<St, WebRequest<In>, Res = WebRequest<Out>, Error = WebError<St, St::Error>>,
{
    type Res = WebResponse;
    type Error = WebError<St, St::Error>;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.ready(&self.filter).await
    }

    async fn call(
        &self,
        req: WebRequest<In>,
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
            } else if let Ok(svc) = sf.create(ctx.st()).await {
                self.cache.borrow_mut().insert(id, svc.clone());
                svc
            } else {
                return Ok(req.into_response(Response::InternalServerError().build()));
            }
        } else {
            if let Some(svc) = &*self.cache_default.borrow() {
                svc.clone()
            } else if let Ok(svc) = self.default.create(ctx.st()).await {
                *self.cache_default.borrow_mut() = Some(svc.clone());
                svc
            } else {
                return Ok(req.into_response(Response::InternalServerError().build()));
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
