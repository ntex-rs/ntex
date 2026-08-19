use std::{cell::RefCell, fmt, io, marker::PhantomData, mem, net, rc::Rc, sync::Arc};

use ntex_io::Io;
use ntex_service::{IntoService, Pipeline, Service, cfg::SharedCfg, state::State};
use ntex_util::{HashMap, future::BoxFuture};

use super::factory::{FactoryService, FactoryServiceType, NetService, ServerService};
use super::{Token, builder::bind_addr, socket::Listener};

#[derive(Clone, Debug)]
pub struct ServiceConfig<St>(pub(super) Rc<RefCell<ServiceConfigInner<St>>>);

#[derive(Debug)]
struct Socket {
    name: String,
    sockets: Vec<(Token, Listener, SharedCfg)>,
}

pub(super) struct ServiceConfigInner<St> {
    token: Token,
    on_start_set: bool,
    on_start: Vec<Box<dyn OnWorkerStart<St>>>,
    sockets: Vec<Socket>,
    backlog: i32,
}

impl<St> fmt::Debug for ServiceConfigInner<St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceConfigInner")
            .field("token", &self.token)
            .field("backlog", &self.backlog)
            .field("sockets", &self.sockets)
            .finish()
    }
}

impl<St> ServiceConfig<St>
where
    St: State<St, Io> + Clone + 'static,
{
    pub(super) fn new(token: Token, backlog: i32) -> Self {
        ServiceConfig(Rc::new(RefCell::new(ServiceConfigInner {
            token,
            backlog,
            sockets: Vec::new(),
            on_start_set: false,
            on_start: vec![on_worker_start(async |_| {
                not_configured();
                Ok::<_, &str>(())
            })],
        })))
    }

    /// Add new service to the server.
    pub fn bind(
        &self,
        name: impl AsRef<str>,
        addr: impl net::ToSocketAddrs,
        cfg: impl Into<SharedCfg>,
    ) -> io::Result<&Self> {
        let mut inner = self.0.borrow_mut();

        let cfg = cfg.into();
        let sockets = bind_addr(addr, inner.backlog)?;
        let socket = Socket {
            name: name.as_ref().to_string(),
            sockets: sockets
                .into_iter()
                .map(|lst| (inner.token.next(), Listener::from_tcp(lst), cfg.clone()))
                .collect(),
        };
        inner.sockets.push(socket);

        Ok(self)
    }

    /// Add new service to the server.
    pub fn listen(
        &self,
        name: impl AsRef<str>,
        lst: net::TcpListener,
        cfg: impl Into<SharedCfg>,
    ) -> &Self {
        let mut inner = self.0.borrow_mut();
        let socket = Socket {
            name: name.as_ref().to_string(),
            sockets: vec![(inner.token.next(), Listener::from_tcp(lst), cfg.into())],
        };
        inner.sockets.push(socket);

        self
    }

    /// Register async service configuration function.
    ///
    /// This function get called during worker runtime configuration stage.
    /// It get executed in the worker thread.
    pub fn on_worker_start<F>(&self, f: F) -> &Self
    where
        F: AsyncFn(ServiceRuntime<St>) -> Result<(), &'static str> + Send + Clone + 'static,
    {
        let mut inner = self.0.borrow_mut();
        if !inner.on_start_set {
            inner.on_start.clear();
            inner.on_start_set = true;
        }
        inner.on_start.push(on_worker_start(f));
        self
    }

    pub(super) fn into_factory(
        self,
    ) -> (
        Token,
        Vec<(Token, String, Listener)>,
        FactoryServiceType<St>,
    ) {
        let mut inner = self.0.borrow_mut();

        let mut sockets = Vec::new();
        let mut names = HashMap::default();
        for (idx, s) in mem::take(&mut inner.sockets).into_iter().enumerate() {
            names.insert(
                s.name.clone(),
                Entry {
                    idx,
                    name: s.name.clone(),
                    tokens: s
                        .sockets
                        .iter()
                        .map(|(token, _, cfg)| (*token, cfg.clone()))
                        .collect(),
                },
            );

            sockets.extend(
                s.sockets
                    .into_iter()
                    .map(|(token, lst, _)| (token, s.name.clone(), lst)),
            );
        }

        (
            inner.token,
            sockets,
            Box::new(ConfiguredService {
                names,
                on_start: mem::take(&mut inner.on_start),
            }),
        )
    }
}

struct ConfiguredService<St> {
    names: HashMap<String, Entry>,
    on_start: Vec<Box<dyn OnWorkerStart<St>>>,
}

impl<St> FactoryService<St> for ConfiguredService<St>
where
    St: State<St, Io> + Clone + 'static,
{
    fn clo(&self) -> FactoryServiceType<St> {
        Box::new(Self {
            names: self.names.clone(),
            on_start: self.on_start.iter().map(|cb| (*cb).clo()).collect(),
        })
    }

    fn create(
        &self,
        st: St,
    ) -> BoxFuture<
        'static,
        Result<Vec<(Box<dyn NetService>, Arc<str>, Vec<(Token, SharedCfg)>)>, &'static str>,
    > {
        // configure services
        let rt = ServiceRuntime::new(st, self.names.clone());
        let on_start: Vec<_> = self
            .on_start
            .iter()
            .map(|cb| cb.run(ServiceRuntime(rt.0.clone())))
            .collect();

        // construct services
        Box::pin(async move {
            for fut in on_start {
                fut.await?;
            }
            rt.validate();

            let names = mem::take(&mut rt.0.borrow_mut().names);
            let mut services = mem::take(&mut rt.0.borrow_mut().services);

            let mut res = Vec::new();
            while let Some(svc) = services.pop() {
                if let Some(svc) = svc {
                    for entry in names.values() {
                        if entry.idx == services.len() {
                            res.push((
                                svc,
                                std::sync::Arc::from(entry.name.clone()),
                                entry.tokens.clone(),
                            ));
                            break;
                        }
                    }
                }
            }
            Ok(res)
        })
    }
}

fn not_configured() {
    log::error!("Service is not configured");
}

pub struct ServiceRuntime<St>(Rc<RefCell<ServiceRuntimeInner<St>>>);

#[derive(Debug, Clone)]
struct Entry {
    idx: usize,
    name: String,
    tokens: Vec<(Token, SharedCfg)>,
}

struct ServiceRuntimeInner<St> {
    st: St,
    names: HashMap<String, Entry>,
    services: Vec<Option<Box<dyn NetService>>>,
}

impl<St> fmt::Debug for ServiceRuntime<St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.0.borrow();
        f.debug_struct("ServiceRuntimer")
            .field("names", &inner.names)
            .finish()
    }
}

impl<St: State<St, Io> + Clone + 'static> ServiceRuntime<St> {
    fn new(st: St, names: HashMap<String, Entry>) -> Self {
        let services = (0..names.len()).map(|_| None).collect();
        ServiceRuntime(Rc::new(RefCell::new(ServiceRuntimeInner {
            st,
            names,
            services,
        })))
    }

    fn validate(&self) {
        let inner = self.0.as_ref().borrow();
        for (name, item) in &inner.names {
            if inner.services[item.idx].is_none() {
                log::error!("Service {name:?} is not configured");
            }
        }
    }

    /// Register service.
    ///
    /// Name of the service must be registered during configuration stage with
    /// `ServiceConfig::bind()` or `ServiceConfig::listen()` methods.
    ///
    /// # Panics
    ///
    /// Panics if service with specified name is registered already
    pub fn service<S>(&self, name: &str, svc: impl IntoService<S, St>) -> &Self
    where
        S: Service<St, Req = Io> + 'static,
    {
        let mut inner = self.0.borrow_mut();
        if let Some(entry) = inner.names.get_mut(name) {
            let idx = entry.idx;
            let pipeline = Pipeline::with_stctl(
                inner.st.clone(),
                inner.st.clone(),
                svc.into_service().map(|_| ()).map_err(|_| ()),
            );
            let svc: Box<dyn NetService> = Box::new(ServerService { pipeline });
            inner.services[idx] = Some(svc);
        } else {
            panic!("Unknown service: {name:?}");
        }
        self
    }
}

trait OnWorkerStart<St>: Send {
    fn clo(&self) -> Box<dyn OnWorkerStart<St>>;

    fn run(&self, rt: ServiceRuntime<St>) -> BoxFuture<'static, Result<(), &'static str>>;
}

struct OnWorkerStartImpl<F, St> {
    f: F,
    st: PhantomData<St>,
}

fn on_worker_start<F, St>(f: F) -> Box<dyn OnWorkerStart<St> + Send>
where
    F: AsyncFn(ServiceRuntime<St>) -> Result<(), &'static str> + Send + Clone + 'static,
    St: 'static,
{
    Box::new(OnWorkerStartImpl { f, st: PhantomData })
}

impl<F, St> OnWorkerStart<St> for OnWorkerStartImpl<F, St>
where
    F: AsyncFn(ServiceRuntime<St>) -> Result<(), &'static str> + Send + Clone + 'static,
    St: 'static,
{
    fn clo(&self) -> Box<dyn OnWorkerStart<St>> {
        Box::new(Self {
            f: self.f.clone(),
            st: PhantomData,
        })
    }

    fn run(&self, rt: ServiceRuntime<St>) -> BoxFuture<'static, Result<(), &'static str>> {
        let f = self.f.clone();
        Box::pin(async move { (f)(rt).await })
    }
}

// SAFETY: Send cannot be provided authomatically because of R param
// but R always get executed in one thread and never leave it
unsafe impl<F, St> Send for OnWorkerStartImpl<F, St> where F: Send {}
