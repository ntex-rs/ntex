use std::{cell::Cell, cell::RefCell, fmt, future::Future, io, marker, mem, net, rc::Rc};

use log::error;

use crate::io::Io;
use crate::service::{self, boxed, ServiceFactory as NServiceFactory};
use crate::util::{BoxFuture, HashMap, PoolId, Ready};

use super::service::{
    BoxedServerService, InternalServiceFactory, ServerMessage, StreamService,
};
use super::{builder::bind_addr, counter::CounterGuard, Token};

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

pub struct ServiceConfig(pub(super) Rc<RefCell<ServiceConfigInner>>);

pub(super) struct ServiceConfigInner {
    pub(super) services: Vec<(String, net::TcpListener)>,
    pub(super) apply: Option<Box<dyn ServiceRuntimeConfiguration + Send>>,
    pub(super) threads: usize,
    pub(super) backlog: i32,
    applied: bool,
}

impl ServiceConfig {
    pub(super) fn new(threads: usize, backlog: i32) -> Self {
        ServiceConfig(Rc::new(RefCell::new(ServiceConfigInner {
            threads,
            backlog,
            services: Vec::new(),
            applied: false,
            apply: Some(Box::new(ConfigWrapper {
                f: |_| {
                    not_configured();
                    Ready::Ok::<_, &'static str>(())
                },
                _t: marker::PhantomData,
            })),
        })))
    }

    /// Add new service to the server.
    pub fn bind<U, N: AsRef<str>>(&self, name: N, addr: U) -> io::Result<&Self>
    where
        U: net::ToSocketAddrs,
    {
        let sockets = bind_addr(addr, self.0.borrow().backlog)?;

        for lst in sockets {
            self.listen(name.as_ref(), lst);
        }

        Ok(self)
    }

    /// Add new service to the server.
    pub fn listen<N: AsRef<str>>(&self, name: N, lst: net::TcpListener) -> &Self {
        {
            let mut inner = self.0.borrow_mut();
            if !inner.applied {
                inner.apply = Some(Box::new(ConfigWrapper {
                    f: |_| {
                        not_configured();
                        Ready::Ok::<_, &'static str>(())
                    },
                    _t: marker::PhantomData,
                }));
            }
            inner.services.push((name.as_ref().to_string(), lst));
        }
        self
    }

    /// Register async service configuration function.
    ///
    /// This function get called during worker runtime configuration stage.
    /// It get executed in the worker thread.
    pub fn on_worker_start<F, R, E>(&self, f: F) -> io::Result<()>
    where
        F: Fn(ServiceRuntime) -> R + Send + Clone + 'static,
        R: Future<Output = Result<(), E>> + 'static,
        E: fmt::Display + 'static,
    {
        self.0.borrow_mut().applied = true;
        self.0.borrow_mut().apply = Some(Box::new(ConfigWrapper {
            f,
            _t: marker::PhantomData,
        }));
        Ok(())
    }
}

pub(super) struct ConfiguredService {
    rt: Box<dyn ServiceRuntimeConfiguration + Send>,
    names: HashMap<Token, (String, net::SocketAddr)>,
    topics: HashMap<String, Token>,
    services: Vec<Token>,
}

impl ConfiguredService {
    pub(super) fn new(rt: Box<dyn ServiceRuntimeConfiguration + Send>) -> Self {
        ConfiguredService {
            rt,
            names: HashMap::default(),
            topics: HashMap::default(),
            services: Vec::new(),
        }
    }

    pub(super) fn stream(&mut self, token: Token, name: String, addr: net::SocketAddr) {
        self.names.insert(token, (name.clone(), addr));
        self.topics.insert(name, token);
        self.services.push(token);
    }
}

impl InternalServiceFactory for ConfiguredService {
    fn name(&self, token: Token) -> &str {
        &self.names[&token].0
    }

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory> {
        Box::new(Self {
            rt: self.rt.clone(),
            names: self.names.clone(),
            topics: self.topics.clone(),
            services: self.services.clone(),
        })
    }

    fn create(&self) -> BoxFuture<'static, Result<Vec<(Token, BoxedServerService)>, ()>> {
        // configure services
        let rt = ServiceRuntime::new(self.topics.clone());
        let cfg_fut = self.rt.configure(ServiceRuntime(rt.0.clone()));
        let mut names = self.names.clone();
        let tokens = self.services.clone();

        // construct services
        Box::pin(async move {
            cfg_fut.await?;
            rt.validate();

            let mut services = mem::take(&mut rt.0.borrow_mut().services);
            // TODO: Proper error handling here
            let onstart = mem::take(&mut rt.0.borrow_mut().onstart);
            for f in onstart.into_iter() {
                f.await;
            }
            let mut res = vec![];
            for token in tokens {
                if let Some(srv) = services.remove(&token) {
                    let newserv = srv.create(());
                    match newserv.await {
                        Ok(serv) => {
                            res.push((token, serv));
                        }
                        Err(_) => {
                            error!("Cannot construct service");
                            return Err(());
                        }
                    }
                } else {
                    let name = names.remove(&token).unwrap().0;
                    res.push((
                        token,
                        boxed::service(StreamService::new(
                            service::fn_service(move |_: Io| {
                                error!("Service {:?} is not configured", name);
                                Ready::<_, ()>::Ok(())
                            }),
                            PoolId::P0,
                        )),
                    ));
                };
            }
            Ok(res)
        })
    }
}

pub(super) trait ServiceRuntimeConfiguration {
    fn clone(&self) -> Box<dyn ServiceRuntimeConfiguration + Send>;

    fn configure(&self, rt: ServiceRuntime) -> BoxFuture<'static, Result<(), ()>>;
}

pub(super) struct ConfigWrapper<F, R, E> {
    pub(super) f: F,
    pub(super) _t: marker::PhantomData<(R, E)>,
}

// SAFETY: we dont store R or E in ConfigWrapper
unsafe impl<F: Send, R, E> Send for ConfigWrapper<F, R, E> {}

impl<F, R, E> ServiceRuntimeConfiguration for ConfigWrapper<F, R, E>
where
    F: Fn(ServiceRuntime) -> R + Send + Clone + 'static,
    R: Future<Output = Result<(), E>> + 'static,
    E: fmt::Display + 'static,
{
    fn clone(&self) -> Box<dyn ServiceRuntimeConfiguration + Send> {
        Box::new(ConfigWrapper {
            f: self.f.clone(),
            _t: marker::PhantomData,
        })
    }

    fn configure(&self, rt: ServiceRuntime) -> BoxFuture<'static, Result<(), ()>> {
        let f = self.f.clone();
        Box::pin(async move {
            (f)(rt).await.map_err(|e| {
                error!("Cannot configure service: {}", e);
            })
        })
    }
}

fn not_configured() {
    error!("Service is not configured");
}

pub struct ServiceRuntime(Rc<RefCell<ServiceRuntimeInner>>);

struct ServiceRuntimeInner {
    names: HashMap<String, Token>,
    services: HashMap<Token, BoxServiceFactory>,
    onstart: Vec<BoxFuture<'static, ()>>,
}

impl ServiceRuntime {
    fn new(names: HashMap<String, Token>) -> Self {
        ServiceRuntime(Rc::new(RefCell::new(ServiceRuntimeInner {
            names,
            services: HashMap::default(),
            onstart: Vec::new(),
        })))
    }

    fn validate(&self) {
        let inner = self.0.as_ref().borrow();
        for (name, token) in &inner.names {
            if !inner.services.contains_key(token) {
                error!("Service {:?} is not configured", name);
            }
        }
    }

    /// Register service.
    ///
    /// Name of the service must be registered during configuration stage with
    /// *ServiceConfig::bind()* or *ServiceConfig::listen()* methods.
    pub fn service<T, F>(&self, name: &str, service: F)
    where
        F: service::IntoServiceFactory<T, Io>,
        T: service::ServiceFactory<Io> + 'static,
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
        F: service::IntoServiceFactory<T, Io>,
        T: service::ServiceFactory<Io> + 'static,
        T::Service: 'static,
        T::InitError: fmt::Debug,
    {
        let mut inner = self.0.borrow_mut();
        if let Some(token) = inner.names.get(name) {
            let token = *token;
            inner.services.insert(
                token,
                boxed::factory(ServiceFactory {
                    pool,
                    inner: service.into_factory(),
                }),
            );
        } else {
            panic!("Unknown service: {:?}", name);
        }
    }

    /// Execute future before services initialization.
    pub fn on_start<F>(&self, fut: F)
    where
        F: Future<Output = ()> + 'static,
    {
        self.0.borrow_mut().onstart.push(Box::pin(fut))
    }
}

type BoxServiceFactory = service::boxed::BoxServiceFactory<
    (),
    (Option<CounterGuard>, ServerMessage),
    (),
    (),
    (),
>;

struct ServiceFactory<T> {
    inner: T,
    pool: PoolId,
}

impl<T> service::ServiceFactory<(Option<CounterGuard>, ServerMessage)> for ServiceFactory<T>
where
    T: service::ServiceFactory<Io>,
    T::Service: 'static,
    T::Error: 'static,
    T::InitError: fmt::Debug + 'static,
{
    type Response = ();
    type Error = ();
    type InitError = ();
    type Service = BoxedServerService;
    type Future<'f> = BoxFuture<'f, Result<BoxedServerService, ()>> where Self: 'f;

    fn create(&self, _: ()) -> Self::Future<'_> {
        let pool = self.pool;
        let fut = self.inner.create(());
        Box::pin(async move {
            match fut.await {
                Ok(s) => Ok(boxed::service(StreamService::new(s, pool))),
                Err(e) => {
                    error!("Cannot construct service: {:?}", e);
                    Err(())
                }
            }
        })
    }
}
