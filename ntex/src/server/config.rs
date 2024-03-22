use std::{cell::Cell, cell::RefCell, fmt, future::Future, io, marker, mem, net, rc::Rc};

use crate::io::Io;
use crate::service::{IntoServiceFactory, ServiceFactory};
use crate::util::{BoxFuture, HashMap, PoolId, Ready};

use super::factory::{
    self, BoxServerService, FactoryService, FactoryServiceType, NetService,
};
use super::{builder::bind_addr, socket::Listener, Token};

#[derive(Clone, Debug)]
pub struct Config(Rc<InnerServiceConfig>);

#[derive(Debug)]
pub(super) struct InnerServiceConfig {
    pub(super) pool: Cell<PoolId>,
}

impl Default for Config {
    fn default() -> Self {
        Self(Rc::new(InnerServiceConfig {
            pool: Cell::new(PoolId::DEFAULT),
        }))
    }
}

impl Config {
    /// Set memory pool for the service.
    ///
    /// Use specified memory pool for memory allocations.
    pub fn memory_pool(&self, id: PoolId) -> &Self {
        self.0.pool.set(id);
        self
    }

    pub(super) fn get_pool_id(&self) -> PoolId {
        self.0.pool.get()
    }
}

#[derive(Clone)]
pub struct ServiceConfig(pub(super) Rc<RefCell<ServiceConfigInner>>);

struct Socket {
    name: String,
    sockets: Vec<(Token, Listener, &'static str)>,
}

pub(super) struct ServiceConfigInner {
    token: Token,
    apply: Option<Box<dyn OnWorkerStart>>,
    sockets: Vec<Socket>,
    backlog: i32,
}

impl ServiceConfig {
    pub(super) fn new(token: Token, backlog: i32) -> Self {
        ServiceConfig(Rc::new(RefCell::new(ServiceConfigInner {
            token,
            backlog,
            sockets: Vec::new(),
            apply: Some(OnWorkerStartWrapper::create(|_| {
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
            sockets: sockets
                .into_iter()
                .map(|lst| (inner.token.next(), Listener::Tcp(lst), ""))
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
            sockets: vec![(inner.token.next(), Listener::Tcp(lst), "")],
        };
        inner.sockets.push(socket);

        self
    }

    /// Set io tag for configured service.
    pub fn set_tag<N: AsRef<str>>(&self, name: N, tag: &'static str) -> &Self {
        let mut inner = self.0.borrow_mut();
        for sock in &mut inner.sockets {
            if sock.name == name.as_ref() {
                for item in &mut sock.sockets {
                    item.2 = tag;
                }
            }
        }
        self
    }

    /// Register async service configuration function.
    ///
    /// This function get called during worker runtime configuration stage.
    /// It get executed in the worker thread.
    pub fn on_worker_start<F, R, E>(&self, f: F) -> &Self
    where
        F: Fn(ServiceRuntime) -> R + Send + Clone + 'static,
        R: Future<Output = Result<(), E>> + 'static,
        E: fmt::Display + 'static,
    {
        self.0.borrow_mut().apply = Some(OnWorkerStartWrapper::create(f));
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
                    pool: PoolId::DEFAULT,
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
                rt: inner.apply.take().unwrap(),
                names,
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
            while let Some(Some(svc)) = services.pop() {
                for entry in names.values() {
                    if entry.idx == services.len() {
                        res.push(NetService {
                            pool: entry.pool,
                            tokens: entry.tokens.clone(),
                            factory: svc,
                        });
                        break;
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
    pool: PoolId,
    tokens: Vec<(Token, &'static str)>,
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
                log::error!("Service {:?} is not configured", name);
            }
        }
    }

    /// Register service.
    ///
    /// Name of the service must be registered during configuration stage with
    /// *ServiceConfig::bind()* or *ServiceConfig::listen()* methods.
    pub fn service<T, F>(&self, name: &str, service: F)
    where
        F: IntoServiceFactory<T, Io>,
        T: ServiceFactory<Io> + 'static,
        T::Service: 'static,
        T::InitError: fmt::Debug,
    {
        self.service_in(name, PoolId::P0, service)
    }

    /// Register service with memory pool.
    ///
    /// Name of the service must be registered during configuration stage with
    /// *ServiceConfig::bind()* or *ServiceConfig::listen()* methods.
    pub fn service_in<T, F>(&self, name: &str, pool: PoolId, service: F)
    where
        F: IntoServiceFactory<T, Io>,
        T: ServiceFactory<Io> + 'static,
        T::Service: 'static,
        T::InitError: fmt::Debug,
    {
        let mut inner = self.0.borrow_mut();
        if let Some(entry) = inner.names.get_mut(name) {
            let idx = entry.idx;
            entry.pool = pool;
            inner.services[idx] = Some(factory::create_boxed_factory(
                name.to_string(),
                service.into_factory(),
            ));
        } else {
            panic!("Unknown service: {:?}", name);
        }
    }
}

trait OnWorkerStart: Send {
    fn clone(&self) -> Box<dyn OnWorkerStart>;

    fn run(&self, rt: ServiceRuntime) -> BoxFuture<'static, Result<(), ()>>;
}

struct OnWorkerStartWrapper<F, R, E> {
    pub(super) f: F,
    pub(super) _t: marker::PhantomData<(R, E)>,
}

impl<F, R, E> OnWorkerStartWrapper<F, R, E>
where
    F: Fn(ServiceRuntime) -> R + Send + Clone + 'static,
    R: Future<Output = Result<(), E>> + 'static,
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
unsafe impl<F, R, E> Send for OnWorkerStartWrapper<F, R, E> where F: Send {}

impl<F, R, E> OnWorkerStart for OnWorkerStartWrapper<F, R, E>
where
    F: Fn(ServiceRuntime) -> R + Send + Clone + 'static,
    R: Future<Output = Result<(), E>> + 'static,
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
                log::error!("On worker start callback failed: {}", e);
            })
        })
    }
}
