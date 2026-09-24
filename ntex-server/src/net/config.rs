use std::{cell::RefCell, fmt, io, marker::PhantomData, mem, net, rc::Rc, sync::Arc};

use ntex_io::Io;
use ntex_service::{IntoService, Pipeline, Service, cfg::SharedCfg};
use ntex_util::{HashMap, future::BoxFuture};

use super::factory::{FactoryService, FactoryServiceType, NetService, ServerService};
use super::{ServerAppConfig, Token, builder::bind_addr, socket::Listener};

/// Listener and worker configuration used by [`ServerBuilder::configure`].
///
/// Listeners are registered by name. Each worker then runs the
/// [`on_worker_start`](Self::on_worker_start) callbacks, which attach a
/// service to each name through [`ServiceRuntime::service`].
///
/// [`ServerBuilder::configure`]: super::ServerBuilder::configure
#[derive(Debug)]
pub struct ServiceConfig<Cfg: ServerAppConfig>(pub(super) Rc<RefCell<ServiceConfigInner<Cfg>>>);

#[derive(Debug)]
struct Socket {
    name: String,
    sockets: Vec<(Token, Listener, SharedCfg)>,
}

pub(super) struct ServiceConfigInner<Cfg: ServerAppConfig> {
    token: Token,
    on_start_set: bool,
    on_start: Vec<Box<dyn OnWorkerStart<Cfg::State>>>,
    sockets: Vec<Socket>,
    backlog: i32,
}

impl<Cfg: ServerAppConfig> fmt::Debug for ServiceConfigInner<Cfg> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceConfigInner")
            .field("token", &self.token)
            .field("backlog", &self.backlog)
            .field("sockets", &self.sockets)
            .finish()
    }
}

impl<Cfg: ServerAppConfig> Clone for ServiceConfig<Cfg> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Cfg: ServerAppConfig> ServiceConfig<Cfg> {
    pub(super) fn new(token: Token, backlog: i32) -> Self {
        ServiceConfig(Rc::new(RefCell::new(ServiceConfigInner {
            token,
            backlog,
            sockets: Vec::new(),
            on_start_set: false,
            on_start: vec![on_worker_start(async |_| {
                not_configured();
                Ok(())
            })],
        })))
    }

    /// Binds TCP listeners under the specified name.
    ///
    /// A listener is created for every address resolved from `addr`, using
    /// the builder's backlog. Binding succeeds if at least one address binds.
    /// The service for `name` is attached later with
    /// [`ServiceRuntime::service`].
    pub fn bind(&self, name: impl AsRef<str>, addr: impl net::ToSocketAddrs) -> io::Result<&Self> {
        let mut inner = self.0.borrow_mut();

        let sockets = bind_addr(addr, inner.backlog)?;
        let socket = Socket {
            name: name.as_ref().to_string(),
            sockets: sockets
                .into_iter()
                .map(|lst| {
                    (
                        inner.token.next(),
                        Listener::from_tcp(lst),
                        SharedCfg::default(),
                    )
                })
                .collect(),
        };
        inner.sockets.push(socket);

        Ok(self)
    }

    /// Registers an existing TCP listener under the specified name.
    ///
    /// The listener is switched to non-blocking mode. The service for `name`
    /// is attached later with [`ServiceRuntime::service`].
    pub fn listen(&self, name: impl AsRef<str>, lst: net::TcpListener) -> &Self {
        let mut inner = self.0.borrow_mut();
        let socket = Socket {
            name: name.as_ref().to_string(),
            sockets: vec![(
                inner.token.next(),
                Listener::from_tcp(lst),
                SharedCfg::default(),
            )],
        };
        inner.sockets.push(socket);

        self
    }

    /// Registers an asynchronous worker configuration callback.
    ///
    /// The callback runs on each worker thread while the worker creates its
    /// services, and it should attach a service to every registered name.
    /// Multiple callbacks run in registration order. An error fails the
    /// worker's service creation. Names left without a service are logged
    /// as errors, and their connections are dropped.
    pub fn on_worker_start<F>(&self, f: F) -> &Self
    where
        F: AsyncFnOnce(ServiceRuntime<Cfg::State>) -> io::Result<()> + Send + Clone + 'static,
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
        FactoryServiceType<Cfg>,
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

/// Per-worker runtime used to attach services to named listeners.
///
/// Passed to [`ServiceConfig::on_worker_start`] callbacks.
pub struct ServiceRuntime<Cfg>(Cfg, Rc<RefCell<ServiceRuntimeInner>>);

#[derive(Debug, Clone)]
struct Entry {
    idx: usize,
    name: String,
    tokens: Vec<(Token, SharedCfg)>,
}

struct ServiceRuntimeInner {
    names: HashMap<String, Entry>,
    services: Vec<Option<Box<dyn NetService>>>,
}

impl<Cfg> fmt::Debug for ServiceRuntime<Cfg> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.1.borrow();
        f.debug_struct("ServiceRuntimer")
            .field("names", &inner.names)
            .finish()
    }
}

impl<Cfg: Clone + 'static> ServiceRuntime<Cfg> {
    fn new(st: Cfg, names: HashMap<String, Entry>) -> Self {
        let services = (0..names.len()).map(|_| None).collect();
        ServiceRuntime(
            st,
            Rc::new(RefCell::new(ServiceRuntimeInner { names, services })),
        )
    }

    fn validate(&self) {
        let inner = self.1.as_ref().borrow();
        for (name, item) in &inner.names {
            if inner.services[item.idx].is_none() {
                log::error!("Service {name:?} is not configured");
            }
        }
    }

    /// Returns the application state passed to services.
    pub fn cfg(&self) -> &Cfg {
        &self.0
    }

    /// Attaches a service to the listeners registered under `name`.
    ///
    /// The name must be registered during the configuration stage with
    /// [`ServiceConfig::bind`] or [`ServiceConfig::listen`]. `cfg` is the I/O
    /// configuration for connections accepted by those listeners. Attaching
    /// a service to the same name again replaces the previous one.
    ///
    /// # Panics
    ///
    /// Panics if no listener is registered under `name`.
    pub fn service<S>(
        &self,
        name: &str,
        cfg: impl Into<SharedCfg>,
        svc: impl IntoService<S, Cfg, Io>,
    ) -> &Self
    where
        S: Service<Cfg, Io> + 'static,
    {
        let shared = cfg.into();
        let mut inner = self.1.borrow_mut();
        if let Some(entry) = inner.names.get_mut(name) {
            let idx = entry.idx;
            for token in &mut entry.tokens {
                token.1 = shared.clone();
            }
            let pipeline = Pipeline::new(
                self.0.clone(),
                svc.into_service().map(|_| ()).map_err(|_| ()),
            );
            let svc: Box<dyn NetService> = Box::new(ServerService { pipeline });
            inner.services[idx] = Some(svc);
        } else {
            panic!("Unknown service: {name:?}");
        }
        self
    }

    /// Returns a runtime that passes `st` as the state to services.
    ///
    /// Services registered through the returned runtime share the same
    /// listeners as this runtime.
    pub fn map_cfg<T>(&self, st: T) -> ServiceRuntime<T>
    where
        T: Clone + 'static,
    {
        ServiceRuntime(st, self.1.clone())
    }
}

struct ConfiguredService<Cfg: ServerAppConfig> {
    names: HashMap<String, Entry>,
    on_start: Vec<Box<dyn OnWorkerStart<Cfg::State>>>,
}

impl<Cfg: ServerAppConfig> FactoryService<Cfg> for ConfiguredService<Cfg> {
    fn clo(&self) -> FactoryServiceType<Cfg> {
        Box::new(Self {
            names: self.names.clone(),
            on_start: self.on_start.iter().map(|cb| (*cb).clo()).collect(),
        })
    }

    fn create(
        &self,
        st: Cfg::State,
    ) -> BoxFuture<'static, io::Result<Vec<(Box<dyn NetService>, Arc<str>, Vec<(Token, SharedCfg)>)>>>
    {
        // configure services
        let rt = ServiceRuntime::new(st.clone(), self.names.clone());
        let on_start: Vec<_> = self
            .on_start
            .iter()
            .map(|cb| cb.run(ServiceRuntime(st.clone(), rt.1.clone())))
            .collect();

        // construct services
        Box::pin(async move {
            for fut in on_start {
                fut.await?;
            }
            rt.validate();

            let names = mem::take(&mut rt.1.borrow_mut().names);
            let mut services = mem::take(&mut rt.1.borrow_mut().services);

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

trait OnWorkerStart<Cfg>: Send {
    fn clo(&self) -> Box<dyn OnWorkerStart<Cfg>>;

    fn run(&self, rt: ServiceRuntime<Cfg>) -> BoxFuture<'static, io::Result<()>>;
}

struct OnWorkerStartImpl<F, Cfg> {
    f: F,
    st: PhantomData<Cfg>,
}

fn on_worker_start<F, Cfg>(f: F) -> Box<dyn OnWorkerStart<Cfg> + Send>
where
    F: AsyncFnOnce(ServiceRuntime<Cfg>) -> io::Result<()> + Send + Clone + 'static,
    Cfg: 'static,
{
    Box::new(OnWorkerStartImpl { f, st: PhantomData })
}

impl<F, Cfg> OnWorkerStart<Cfg> for OnWorkerStartImpl<F, Cfg>
where
    F: AsyncFnOnce(ServiceRuntime<Cfg>) -> io::Result<()> + Send + Clone + 'static,
    Cfg: 'static,
{
    fn clo(&self) -> Box<dyn OnWorkerStart<Cfg>> {
        Box::new(Self {
            f: self.f.clone(),
            st: PhantomData,
        })
    }

    fn run(&self, rt: ServiceRuntime<Cfg>) -> BoxFuture<'static, io::Result<()>> {
        let f = self.f.clone();
        Box::pin(async move { (f)(rt).await })
    }
}

// SAFETY: Send cannot be provided authomatically because of R param
// but R always get executed in one thread and never leave it
unsafe impl<F, Cfg> Send for OnWorkerStartImpl<F, Cfg> where F: Send {}
