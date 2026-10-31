use std::{cell::Cell, cell::RefCell, marker::PhantomData, mem, rc::Rc};

use crate::error::Failure;
use crate::http::{Request, Response};
use crate::router::{Path, ResourceDef, Router};
use crate::service::cfg::{Cfg, Configuration};
use crate::service::{ReadyCallback, ReadyNotifier, ServiceBinding, ServiceChainFactory, boxed};
use crate::{BindCtx, Ctx, Middleware, Service, ServiceFactory, factory, util::HashMap};

use super::config::WebAppConfig;
use super::guard::Guard;
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, WebServiceConfig};
use super::{HttpHandler, HttpRequest, HttpService, State, WebError, WebRequest, WebResponse};

type Guards = Vec<Box<dyn Guard>>;

/// Service factory to convert `Request` to a `WebRequest`.
/// It also executes state factories.
#[derive(derive_more::Debug)]
#[debug("AppFactory")]
pub struct AppFactory<St, In, Out, M, F>
where
    St: State,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<WebServiceRouter<St, In, Out, F::Service>, St> + 'static,
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
    St: State,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<WebServiceRouter<St, In, Out, F::Service>, St> + 'static,
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
            router
                .resource(path.clone(), factory)
                .set_check_value(guards.borrow_mut().take());
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
    St: State,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<WebServiceRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    type Res = Response;
    type Error = WebError<St, St::Error>;

    type Service = AppService<St, M::Service>;
    type InitError = Failure;

    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        // main service
        let service = WebServiceRouter::new(
            self.filter.create(st).await?,
            self.router.clone(),
            self.default.clone(),
        );

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
    St: State,
    F: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    service: F,
    rmap: Rc<ResourceMap>,
    config: Option<Cfg<WebAppConfig>>,
    _t: PhantomData<St>,
}

impl<St, F> Service<St, Request> for AppService<St, F>
where
    St: State,
    F: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    type Res = Response;
    type Error = F::Error;

    crate::forward_bind!(St, service);
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
            // the pool is shared by all applications that use this config
            if !Rc::ptr_eq(&inner.rmap, &self.rmap) {
                inner.rmap = self.rmap.clone();
            }
            req
        } else {
            HttpRequest::new(Path::new(head.uri.clone()), head, self.rmap.clone(), config)
        };
        match ctx
            .call(&self.service, WebRequest::new(req, payload, ()))
            .await
        {
            Ok(r) => Ok(r.into()),
            Err(e) => Ok(e.0.error_response(ctx.st())),
        }
    }
}

/// Web app service.
///
/// Routes and the default service are created lazily through [`Ctx::create`],
/// binding their dependencies to the current scope before first use.
/// Readiness requires the filter and every cached service to be ready. Uncreated
/// services are excluded. A cached success takes constant time; notifications
/// identify which services need another check without scanning the handler cache.
#[derive(derive_more::Debug)]
#[debug("Router")]
pub struct WebServiceRouter<St: State, In, Out, F> {
    filter: ServiceBinding<CachedService<F>>,
    router: Rc<Router<HttpService<St, Out>, Guards>>,
    default: HttpService<St, Out>,
    cache: RefCell<HashMap<usize, Rc<ServiceBinding<CachedService<HttpHandler<St, Out>>>>>>,
    cache_default: RefCell<Option<Rc<ServiceBinding<CachedService<HttpHandler<St, Out>>>>>>,
    readiness: Rc<RouterReadiness>,
    ph: PhantomData<In>,
}

impl<St: State, In, Out, F> WebServiceRouter<St, In, Out, F> {
    pub fn new(
        filter: F,
        router: Rc<Router<HttpService<St, Out>, Guards>>,
        default: HttpService<St, Out>,
    ) -> Self {
        let readiness = RouterReadiness::new();
        let filter = ServiceBinding::with_data(
            CachedService::new(usize::MAX, filter, &readiness),
            usize::MAX,
        );

        Self {
            filter,
            router,
            default,
            readiness,
            cache: RefCell::new(HashMap::default()),
            cache_default: RefCell::new(None),
            ph: PhantomData,
        }
    }
}

impl<St, In, Out, F> Service<St, WebRequest<In>> for WebServiceRouter<St, In, Out, F>
where
    St: State,
    Out: 'static,
    F: Service<St, WebRequest<In>, Res = WebRequest<Out>, Error = WebError<St, St::Error>>,
{
    type Res = WebResponse;
    type Error = WebError<St, St::Error>;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.readiness.changed);
        ctx.bind::<_, St, WebRequest<In>>(&self.filter);
        for service in self.cache.borrow().values() {
            ctx.bind::<_, St, WebRequest<Out>>(service);
        }
        if let Some(service) = &*self.cache_default.borrow() {
            ctx.bind::<_, St, WebRequest<Out>>(service);
        }
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        if self.readiness.ready.get() {
            return Ok(());
        }

        loop {
            let revision = self.readiness.revision.get();

            ctx.ready(&self.filter).await?;
            let default = self.cache_default.borrow().clone();
            if let Some(default) = default {
                ctx.ready(&default).await?;
            }

            loop {
                let id = self.readiness.unknown.borrow_mut().pop();
                if let Some(id) = id {
                    let hnd = self.cache.borrow().get(&id).cloned();
                    if let Some(hnd) = hnd {
                        if let Err(err) = ctx.ready(&hnd).await {
                            self.readiness.unknown.borrow_mut().push(id);
                            return Err(err);
                        }
                    }
                } else {
                    break;
                }
            }

            if self.readiness.revision.get() == revision {
                self.readiness.ready.set(true);
                return Ok(());
            }
        }
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

        let (sf, id) = res.map_or((&self.default, None), |(sf, id)| (sf, Some(id)));
        let cached = if let Some(id) = id {
            self.cache.borrow().get(&(id.get() as usize)).cloned()
        } else {
            self.cache_default.borrow().clone()
        };

        let svc = if let Some(svc) = cached {
            svc
        } else if let Ok(service) = ctx
            .create(&factory(sf.clone()).apply(CachedServiceMiddleware(
                id.map(|id| id.get() as usize).unwrap_or(usize::MAX),
                self.readiness.clone(),
            )))
            .await
        {
            let svc = Rc::new(service);
            if let Some(id) = id {
                self.cache
                    .borrow_mut()
                    .insert(id.get() as usize, svc.clone());
            } else {
                *self.cache_default.borrow_mut() = Some(svc.clone());
            }
            svc
        } else {
            return Ok(req.into_response(Response::InternalServerError().build()));
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

struct RouterReadiness {
    ready: Cell<bool>,
    revision: Cell<u64>,
    changed: ReadyNotifier,
    unknown: RefCell<Vec<usize>>,
    callback: ReadyCallback,
}

impl RouterReadiness {
    fn new() -> Rc<Self> {
        Rc::<Self>::new_cyclic(|readiness| {
            let readiness = readiness.clone();
            Self {
                ready: Cell::new(true),
                revision: Cell::new(0),
                unknown: RefCell::new(Vec::new()),
                changed: ReadyNotifier::new(),
                callback: ReadyCallback::new(move |id| {
                    if let Some(readiness) = readiness.upgrade() {
                        readiness.set_unknown(id);
                    }
                }),
            }
        })
    }

    fn set_unknown(&self, id: usize) {
        self.ready.set(false);
        self.revision.set(
            self.revision
                .get()
                .checked_add(1)
                .expect("router readiness revision exhausted"),
        );
        if id != 0 {
            self.unknown.borrow_mut().push(id);
        }
    }
}

struct CachedServiceMiddleware(usize, Rc<RouterReadiness>);

impl<S, St> Middleware<S, St> for CachedServiceMiddleware {
    type Service = ServiceBinding<CachedService<S>>;

    fn create(&self, _: &St, service: S) -> Self::Service {
        ServiceBinding::with_data(CachedService::new(self.0, service, &self.1), self.0)
    }
}

struct CachedService<S> {
    service: S,
    changed: ReadyCallback,
}

impl<S> CachedService<S> {
    fn new(id: usize, service: S, readiness: &Rc<RouterReadiness>) -> Self {
        readiness.set_unknown(id);
        readiness.changed.notify();
        CachedService {
            service,
            changed: readiness.callback.clone(),
        }
    }
}

impl<S: Service<St, Req>, St, Req> Service<St, Req> for CachedService<S> {
    type Res = S::Res;
    type Error = S::Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.on_notify(&self.changed);
        ctx.bind(&self.service);
    }

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        ctx.call(&self.service, req).await
    }

    crate::forward_ready!(St, self.service);
    crate::forward_shutdown!(St, self.service);
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use crate::service::{Pipeline, fn_factory};
    use crate::util::join;
    use crate::web::test::TestRequest;

    use super::*;

    #[derive(Default)]
    struct Gate {
        blocked: Cell<bool>,
        failed: Cell<bool>,
        notify_on_check: Cell<bool>,
        checks: Cell<usize>,
        changed: ReadyNotifier,
        waker: RefCell<Option<Waker>>,
    }

    struct Gated(Rc<Gate>);

    impl Service<(), WebRequest<()>> for Gated {
        type Res = WebRequest<()>;
        type Error = WebError;

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.subscribe(&self.0.changed);
        }

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            self.0.checks.set(self.0.checks.get() + 1);
            if self.0.notify_on_check.replace(false) {
                self.0.changed.notify();
            }
            poll_fn(|cx| {
                if self.0.failed.get() {
                    Poll::Ready(Err(WebError::from_err(std::io::Error::other("not ready"))))
                } else if self.0.blocked.get() {
                    *self.0.waker.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            })
            .await
        }

        async fn call(
            &self,
            req: WebRequest<()>,
            _: Ctx<'_, Self>,
        ) -> Result<Self::Res, Self::Error> {
            Ok(req)
        }
    }

    type TestRouter = WebServiceRouter<(), (), (), Gated>;

    fn handler(gate: Rc<Gate>) -> HttpService<(), ()> {
        boxed::factory(
            factory(fn_factory(async move |(): &()| {
                Ok::<_, Failure>(Gated(gate.clone()))
            }))
            .map(|req: WebRequest<()>| req.into_response(Response::Ok().build())),
        )
    }

    fn router(filter: Rc<Gate>, gates: &[Rc<Gate>], default: Rc<Gate>) -> Rc<TestRouter> {
        let mut routes = Router::builder();
        for (id, gate) in gates.iter().enumerate() {
            routes
                .path(format!("/{id}"), handler(gate.clone()))
                .set_id(id as u16);
        }
        Rc::new(WebServiceRouter::new(
            Gated(filter),
            Rc::new(routes.build()),
            handler(default),
        ))
    }

    struct CheckRouter(Rc<TestRouter>);

    impl Service<(), Option<WebRequest<()>>> for CheckRouter {
        type Res = ();
        type Error = WebError;

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.bind::<_, (), WebRequest<()>>(&self.0);
        }

        async fn call(
            &self,
            req: Option<WebRequest<()>>,
            ctx: Ctx<'_, Self>,
        ) -> Result<(), Self::Error> {
            if let Some(req) = req {
                ctx.call(&self.0, req).await.map(|_| ())
            } else {
                // Exercise the router itself, not the pipeline's readiness cache.
                ctx.ready::<_, WebRequest<()>>(&self.0).await
            }
        }
    }

    /// STATEMENT: Cached aggregate readiness visits no handlers and invalidation targets one entry.
    ///
    /// HOW:
    /// - Cache many routes and a default, leaving one unavailable route uncreated.
    /// - Verify cached handlers share the filter's callback without adding weak router captures.
    /// - Probe raw readiness through the same pipeline with both caches mutably borrowed.
    /// - Notify resource zero and another route repeatedly, checking their tags and targeted rechecks.
    /// - Invalidate the default and filter independently and verify unaffected entries stay cached.
    #[crate::rt_test]
    async fn cached_readiness_targets_changed_entries() {
        let filter = Rc::new(Gate::default());
        let default = Rc::new(Gate::default());
        let gates: Vec<_> = (0..17).map(|_| Rc::new(Gate::default())).collect();
        gates[16].blocked.set(true);
        let router = router(filter.clone(), &gates, default.clone());
        let captures = Rc::weak_count(&router.readiness);
        let srv = Pipeline::new((), CheckRouter(router.clone()));
        for id in 0..16 {
            srv.call(Some(
                TestRequest::with_uri(&format!("/{id}")).to_srv_request(),
            ))
            .await
            .unwrap();
        }
        srv.call(Some(TestRequest::with_uri("/missing").to_srv_request()))
            .await
            .unwrap();
        srv.call(None).await.unwrap();
        assert_eq!(gates[16].checks.get(), 0);
        assert_eq!(Rc::weak_count(&router.readiness), captures);

        let probe = srv.bind();
        for _ in 0..3 {
            let mut check = pin!(probe.call(None));
            let _routes = router.cache.borrow_mut();
            let _default = router.cache_default.borrow_mut();
            assert!(matches!(
                check.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Ok(()))
            ));
        }
        for id in [0, 3] {
            for _ in 0..3 {
                gates[id].changed.notify();
            }
            {
                let unknown = router.readiness.unknown.borrow();
                assert!(!unknown.is_empty());
                assert!(unknown.iter().all(|tag| *tag == id + 1));
            }
            probe.call(None).await.unwrap();
            assert!(router.readiness.unknown.borrow().is_empty());
        }
        for (id, gate) in gates[..16].iter().enumerate() {
            assert_eq!(gate.checks.get(), 1 + usize::from(id == 0 || id == 3));
        }
        assert_eq!(filter.checks.get(), 1);
        assert_eq!(default.checks.get(), 1);

        default.changed.notify();
        probe.call(None).await.unwrap();
        assert_eq!(default.checks.get(), 2);
        filter.changed.notify();
        let mut check = pin!(probe.call(None));
        assert!(matches!(
            check.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(filter.checks.get(), 2);
    }

    /// STATEMENT: Pending aggregate checks survive cancellation and ordinary task wakes.
    ///
    /// HOW:
    /// - Block two cached handlers and start raw checks through independent handles of one pipeline.
    /// - Cancel the first checker, then release one handler through its ordinary task waker.
    /// - Verify readiness still waits for the handler whose original check was cancelled.
    /// - Recover the remaining handler and verify all dirty work is eventually cleared.
    #[crate::rt_test]
    async fn readiness_cancellation_and_recovery() {
        let filter = Rc::new(Gate::default());
        let default = Rc::new(Gate::default());
        let gates = [Rc::new(Gate::default()), Rc::new(Gate::default())];
        let router = router(filter.clone(), &gates, default.clone());
        let srv = Pipeline::new((), CheckRouter(router.clone()));
        for path in ["/0", "/1", "/missing"] {
            srv.call(Some(TestRequest::with_uri(path).to_srv_request()))
                .await
                .unwrap();
        }
        srv.call(None).await.unwrap();
        let other = srv.bind();
        for gate in &gates {
            gate.blocked.set(true);
            gate.changed.notify();
        }
        let mut cx = Context::from_waker(Waker::noop());
        let mut first = Box::pin(srv.call(None));
        assert!(first.as_mut().poll(&mut cx).is_pending());
        let mut second = Box::pin(other.call(None));
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert_eq!((gates[0].checks.get(), gates[1].checks.get()), (2, 2));
        drop(first);
        assert!(second.as_mut().poll(&mut cx).is_pending());

        gates[0].blocked.set(false);
        gates[0].waker.borrow_mut().take().unwrap().wake();
        assert!(second.as_mut().poll(&mut cx).is_pending());
        let checks = gates[0].checks.get();

        gates[1].blocked.set(false);
        gates[1].changed.notify();
        second.await.unwrap();
        srv.call(None).await.unwrap();
        assert!(router.readiness.unknown.borrow().is_empty());
        assert_eq!(gates[0].checks.get(), checks);
        assert_eq!(filter.checks.get(), 1);
    }

    /// STATEMENT: Failed cached handlers are retried until their readiness recovers.
    ///
    /// HOW:
    /// - Cache a route, make its readiness fail, and notify once.
    /// - Require consecutive aggregate checks to return the error without another notification.
    /// - Recover the route with a notification during its next check and verify readiness succeeds.
    #[crate::rt_test]
    async fn readiness_errors_are_retried_until_recovery() {
        let filter = Rc::new(Gate::default());
        let gate = Rc::new(Gate::default());
        let router = router(
            filter.clone(),
            std::slice::from_ref(&gate),
            Rc::new(Gate::default()),
        );
        let srv = Pipeline::new((), router.clone());
        srv.call(TestRequest::with_uri("/0").to_srv_request())
            .await
            .unwrap();
        srv.ready().await.unwrap();

        gate.failed.set(true);
        gate.changed.notify();
        for _ in 0..2 {
            assert!(srv.ready().await.is_err());
        }

        gate.failed.set(false);
        gate.notify_on_check.set(true);
        gate.changed.notify();
        srv.ready().await.unwrap();
        assert!(router.readiness.unknown.borrow().is_empty());
        assert_eq!(filter.checks.get(), 1);
    }

    /// STATEMENT: Nested lazy creation invalidates the enclosing cached handler's aggregate readiness.
    ///
    /// HOW:
    /// - Cache a nested router and one ready endpoint, leaving another blocked endpoint uncreated.
    /// - Start a call that creates the blocked endpoint and verify the outer handler becomes unknown.
    /// - Verify outer readiness waits, then release the new endpoint and complete both futures.
    #[crate::rt_test]
    async fn nested_creation_marks_parent_unknown() {
        let filter = Rc::new(Gate::default());
        let first = Rc::new(Gate::default());
        let second = Rc::new(Gate::default());
        second.blocked.set(true);
        let inner = router(
            Rc::new(Gate::default()),
            &[first.clone(), second.clone()],
            Rc::new(Gate::default()),
        );
        let nested = inner.clone();
        let mut routes = Router::builder();
        routes.prefix(
            "/nested",
            boxed::factory(fn_factory(async move |(): &()| {
                Ok::<_, Failure>(nested.clone())
            })),
        );
        let outer = Rc::new(WebServiceRouter::new(
            Gated(filter.clone()),
            Rc::new(routes.build()),
            handler(Rc::new(Gate::default())),
        ));
        let srv = Pipeline::new((), outer.clone());
        srv.call(TestRequest::with_uri("/nested/0").to_srv_request())
            .await
            .unwrap();
        srv.ready().await.unwrap();
        assert!(outer.readiness.unknown.borrow().is_empty());
        assert_eq!(second.checks.get(), 0);

        let mut call = Box::pin(srv.call(TestRequest::with_uri("/nested/1").to_srv_request()));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(call.as_mut().poll(&mut cx).is_pending());
        assert!(!srv.is_ready());
        assert_eq!(outer.readiness.unknown.borrow().len(), 1);
        assert_eq!(inner.readiness.unknown.borrow().len(), 1);
        let mut ready = Box::pin(srv.ready());
        assert!(ready.as_mut().poll(&mut cx).is_pending());
        second.blocked.set(false);
        second.changed.notify();
        let (res, ready) = join(call, ready).await;
        res.unwrap();
        ready.unwrap();
        assert!(outer.readiness.unknown.borrow().is_empty());
        assert!(inner.readiness.unknown.borrow().is_empty());
        assert_eq!(first.checks.get(), 1);
        assert_eq!(filter.checks.get(), 1);
    }

    /// STATEMENT: Concurrent cache replacement uses the new service despite a retained old service.
    ///
    /// HOW:
    /// - Start three lazy creations for a route or default and cancel one before publication.
    /// - Publish the first service, block it, and start aggregate readiness.
    /// - Publish the second service and verify readiness recovers despite the retained old service.
    /// - Notify the old service again and verify readiness succeeds without rechecking either service.
    #[crate::rt_test]
    async fn concurrent_creation_retires_old_entry() {
        for is_default in [false, true] {
            let gates = [Rc::new(Gate::default()), Rc::new(Gate::default())];
            let releases = Rc::new(RefCell::new(Vec::new()));
            let senders = releases.clone();
            let services = gates.clone();
            let creates = Rc::new(Cell::new(0));
            let attempts = creates.clone();
            let sf = boxed::factory(
                factory(fn_factory(async move |(): &()| {
                    let idx = attempts.get();
                    attempts.set(idx + 1);
                    let (tx, rx) = crate::channel::oneshot::channel();
                    senders.borrow_mut().push(tx);
                    rx.await.unwrap();
                    Ok::<_, Failure>(Gated(services[idx].clone()))
                }))
                .map(|req: WebRequest<()>| req.into_response(Response::Ok().build())),
            );
            let mut routes = Router::builder();
            routes.path("/route", sf.clone());
            let default = if is_default {
                sf
            } else {
                handler(Rc::new(Gate::default()))
            };
            let router = Rc::new(WebServiceRouter::new(
                Gated(Rc::new(Gate::default())),
                Rc::new(routes.build()),
                default,
            ));
            let srv = Pipeline::new((), router.clone());
            let path = if is_default { "/missing" } else { "/route" };
            let mut first = Box::pin(srv.call(TestRequest::with_uri(path).to_srv_request()));
            let mut second = Box::pin(srv.call(TestRequest::with_uri(path).to_srv_request()));
            let mut cancelled = Box::pin(srv.call(TestRequest::with_uri(path).to_srv_request()));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(first.as_mut().poll(&mut cx).is_pending());
            assert!(second.as_mut().poll(&mut cx).is_pending());
            assert!(cancelled.as_mut().poll(&mut cx).is_pending());
            assert_eq!(creates.get(), 3);
            drop(cancelled);
            let mut releases = mem::take(&mut *releases.borrow_mut()).into_iter();
            releases.next().unwrap().send(()).unwrap();
            first.await.unwrap();
            srv.ready().await.unwrap();
            let previous = if is_default {
                router.cache_default.borrow().as_ref().unwrap().clone()
            } else {
                router.cache.borrow().values().next().unwrap().clone()
            };
            gates[0].blocked.set(true);
            gates[0].changed.notify();
            let mut ready = Box::pin(srv.ready());
            assert!(ready.as_mut().poll(&mut cx).is_pending());
            releases.next().unwrap().send(()).unwrap();
            second.await.unwrap();
            ready.await.unwrap();
            assert!(releases.next().unwrap().send(()).is_err());
            let current = if is_default {
                router.cache_default.borrow().as_ref().unwrap().clone()
            } else {
                router.cache.borrow().values().next().unwrap().clone()
            };
            assert!(!Rc::ptr_eq(&previous, &current));
            assert!(router.readiness.unknown.borrow().is_empty());
            let old_checks = gates[0].checks.get();
            gates[0].changed.notify();
            srv.ready().await.unwrap();
            assert!(router.readiness.unknown.borrow().is_empty());
            assert_eq!(gates[0].checks.get(), old_checks);
            assert_eq!(gates[1].checks.get(), 1);
            srv.shutdown().await;
            assert!(router.readiness.unknown.borrow().is_empty());
        }
    }
}
