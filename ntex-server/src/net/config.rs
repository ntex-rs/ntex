use std::{cell::Cell, cell::RefCell, fmt, io, marker, mem, net, rc::Rc};

use ntex_io::Io;
use ntex_service::{IntoServiceFactory, ServiceFactory, cfg::SharedCfg};
use ntex_util::{HashMap, future::BoxFuture, future::Ready};

use super::factory::{
    self, BoxServerService, FactoryService, FactoryServiceType, NetService,
};
use super::{Token, builder::bind_addr, socket::Listener};

#[derive(Clone, Debug)]
pub struct Config(Rc<InnerServiceConfig>);

#[derive(Debug)]
pub(super) struct InnerServiceConfig {
    pub(super) config: Cell<Option<SharedCfg>>,
}

impl Default for Config {
    fn default() -> Self {
        Self(Rc::new(InnerServiceConfig {
            config: Cell::new(None),
        }))
    }
}

impl Config {
    /// Set io config for the service.
    pub fn config<T: Into<SharedCfg>>(&self, cfg: T) -> &Self {
        self.0.config.set(Some(cfg.into()));
        self
    }

    pub(super) fn get_config(&self) -> Option<SharedCfg> {
        self.0.config.get()
    }
}

#[derive(Clone, Debug)]
pub struct ServiceConfig(pub(super) Rc<RefCell<ServiceConfigInner>>);

#[derive(Debug)]
struct Socket {
    name: String,
    config: SharedCfg,
    sockets: Vec<(Token, Listener, SharedCfg)>,
}

pub(super) struct ServiceConfigInner {
    token: Token,
    on_start_set: bool,
    on_start: Option<Box<dyn OnWorkerStart>>,
    sockets: Vec<Socket>,
    backlog: i32,
}

impl fmt::Debug for ServiceConfigInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceConfigInner")
            .field("token", &self.token)
            .field("backlog", &self.backlog)
            .field("sockets", &self.sockets)
            .finish()
    }
}

impl ServiceConfig {
    pub(super) fn new(token: Token, backlog: i32) -> Self {
        ServiceConfig(Rc::new(RefCell::new(ServiceConfigInner {
            token,
            backlog,
            sockets: Vec::new(),
            on_start_set: false,
            on_start: Some(OnWorkerStartWrapper::create(|_| {
                not_configured();
                Ready::Ok::<_, &str>(())
            })),
        })))
    }

    /// Add new service to the server.
    pub fn bind<U, N: AsRef<str>>(&self, name: N, addr: U) -> io::Result<&Self>
    where
        U: net::ToSocketAddrs,
    {
        let mut inner = self.0.borrow_mut();

        let sockets = bind_addr(addr, inner.backlog)?;
        let socket = Socket {
            name: name.as_ref().to_string(),
            config: SharedCfg::default(),
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

    /// Add new service to the server.
    pub fn listen<N: AsRef<str>>(&self, name: N, lst: net::TcpListener) -> &Self {
        let mut inner = self.0.borrow_mut();
        let socket = Socket {
            name: name.as_ref().to_string(),
            config: SharedCfg::default(),
            sockets: vec![(
                inner.token.next(),
                Listener::from_tcp(lst),
                SharedCfg::default(),
            )],
        };
        inner.sockets.push(socket);

        self
    }

    /// Set io config for configured service.
    pub fn config<N, T>(&self, name: N, cfg: T) -> &Self
    where
        N: AsRef<str>,
        T: Into<SharedCfg>,
    {
        let cfg = cfg.into();
        let mut inner = self.0.borrow_mut();
        for sock in &mut inner.sockets {
            if sock.name == name.as_ref() {
                sock.config = cfg;
                for item in &mut sock.sockets {
                    item.2 = cfg;
                }
            }
        }
        self
    }

    /// Register async service configuration function.
    ///
    /// This function get called during worker runtime configuration stage.
    /// It get executed in the worker thread.
    pub fn on_worker_start<F, E>(&self, f: F) -> &Self
    where
        F: AsyncFn(ServiceRuntime) -> Result<(), E> + Send + Clone + 'static,
        E: fmt::Display + 'static,
    {
        let mut inner = self.0.borrow_mut();
        if inner.on_start_set {
            panic!("on_worker_set is already set, can be set once");
        }
        inner.on_start = Some(OnWorkerStartWrapper::create(f));
        self
    }

    pub(super) fn into_factory(
        self,
    ) -> (Token, Vec<(Token, String, Listener)>, FactoryServiceType) {
        let mut inner = self.0.borrow_mut();

        let mut sockets = Vec::new();
        let mut names = HashMap::default();
        for (idx, s) in mem::take(&mut inner.sockets).into_iter().enumerate() {
            names.insert(
                s.name.clone(),
                Entry {
                    idx,
                    name: s.name.clone(),
                    config: s.config,
                    tokens: s
                        .sockets
                        .iter()
                        .map(|(token, _, tag)| (*token, *tag))
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
                rt: inner.on_start.take().unwrap(),
            }),
        )
    }
}

struct ConfiguredService {
    rt: Box<dyn OnWorkerStart>,
    names: HashMap<String, Entry>,
}

impl FactoryService for ConfiguredService {
    fn clone_factory(&self) -> FactoryServiceType {
        Box::new(Self {
            rt: self.rt.clone(),
            names: self.names.clone(),
        })
    }

    fn create(&self) -> BoxFuture<'static, Result<Vec<NetService>, ()>> {
        // configure services
        let rt = ServiceRuntime::new(self.names.clone());
        let cfg_fut = self.rt.run(ServiceRuntime(rt.0.clone()));

        // construct services
        Box::pin(async move {
            cfg_fut.await?;
            rt.validate();

            let names = mem::take(&mut rt.0.borrow_mut().names);
            let mut services = mem::take(&mut rt.0.borrow_mut().services);

            let mut res = Vec::new();
            while let Some(svc) = services.pop() {
                if let Some(svc) = svc {
                    for entry in names.values() {
                        if entry.idx == services.len() {
                            res.push(NetService {
                                config: entry.config,
                                name: std::sync::Arc::from(entry.name.clone()),
                                tokens: entry.tokens.clone(),
                                factory: svc,
                            });
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

pub struct ServiceRuntime(Rc<RefCell<ServiceRuntimeInner>>);

#[derive(Debug, Clone)]
struct Entry {
    idx: usize,
    name: String,
    config: SharedCfg,
    tokens: Vec<(Token, SharedCfg)>,
}

struct ServiceRuntimeInner {
    names: HashMap<String, Entry>,
    services: Vec<Option<BoxServerService>>,
}

impl fmt::Debug for ServiceRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.0.borrow();
        f.debug_struct("ServiceRuntimer")
            .field("names", &inner.names)
            .field("services", &inner.services)
            .finish()
    }
}

impl ServiceRuntime {
    fn new(names: HashMap<String, Entry>) -> Self {
        ServiceRuntime(Rc::new(RefCell::new(ServiceRuntimeInner {
            services: (0..names.len()).map(|_| None).collect(),
            names,
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
    pub fn service<T, F>(&self, name: &str, service: F) -> &Self
    where
        F: IntoServiceFactory<T, Io, SharedCfg>,
        T: ServiceFactory<Io, SharedCfg> + 'static,
        T::Service: 'static,
        T::InitError: fmt::Debug,
    {
        let mut inner = self.0.borrow_mut();
        if let Some(entry) = inner.names.get_mut(name) {
            let idx = entry.idx;
            inner.services[idx] = Some(factory::create_boxed_factory(
                name.to_string(),
                service.into_factory(),
            ));
        } else {
            panic!("Unknown service: {name:?}");
        }
        self
    }

    /// Set io config for configured service.
    pub fn config<T: Into<SharedCfg>>(&self, name: &str, cfg: T) -> &Self {
        let mut inner = self.0.borrow_mut();
        if let Some(entry) = inner.names.get_mut(name) {
            entry.config = cfg.into();
        }
        self
    }
}

trait OnWorkerStart: Send {
    fn clone(&self) -> Box<dyn OnWorkerStart>;

    fn run(&self, rt: ServiceRuntime) -> BoxFuture<'static, Result<(), ()>>;
}

struct OnWorkerStartWrapper<F, E> {
    pub(super) f: F,
    pub(super) _t: marker::PhantomData<E>,
}

impl<F, E> OnWorkerStartWrapper<F, E>
where
    F: AsyncFn(ServiceRuntime) -> Result<(), E> + Send + Clone + 'static,
    E: fmt::Display + 'static,
{
    pub(super) fn create(f: F) -> Box<dyn OnWorkerStart + Send> {
        Box::new(Self {
            f,
            _t: marker::PhantomData,
        })
    }
}

// SAFETY: Send cannot be provided authomatically because of R param
// but R always get executed in one thread and never leave it
unsafe impl<F, E> Send for OnWorkerStartWrapper<F, E> where F: Send {}

impl<F, E> OnWorkerStart for OnWorkerStartWrapper<F, E>
where
    F: AsyncFn(ServiceRuntime) -> Result<(), E> + Send + Clone + 'static,
    E: fmt::Display + 'static,
{
    fn clone(&self) -> Box<dyn OnWorkerStart> {
        Box::new(Self {
            f: self.f.clone(),
            _t: marker::PhantomData,
        })
    }

    fn run(&self, rt: ServiceRuntime) -> BoxFuture<'static, Result<(), ()>> {
        let f = self.f.clone();
        Box::pin(async move {
            (f)(rt).await.map_err(|e| {
                log::error!("On worker start callback failed: {e}");
            })
        })
    }
}
