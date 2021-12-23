use std::{
    cell::Cell, cell::RefCell, fmt, future::Future, io, marker::PhantomData, mem, net,
    pin::Pin, rc::Rc,
};

use log::error;

use crate::util::{counter::CounterGuard, HashMap, Ready};
use crate::{io::Io, service, util::PoolId};

use super::builder::bind_addr;
use super::service::{
    BoxedServerService, InternalServiceFactory, ServerMessage, StreamService,
};
use super::Token;

#[derive(Clone)]
pub struct Config(pub(super) Rc<InnerServiceConfig>);

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
}

pub struct ServiceConfig {
    pub(super) services: Vec<(String, net::TcpListener)>,
    pub(super) apply: Box<dyn ServiceRuntimeConfiguration + Send>,
    pub(super) threads: usize,
    pub(super) backlog: i32,
    applied: bool,
}

impl ServiceConfig {
    pub(super) fn new(threads: usize, backlog: i32) -> Self {
        ServiceConfig {
            threads,
            backlog,
            services: Vec::new(),
            applied: false,
            apply: Box::new(ConfigWrapper {
                f: |_| {
                    not_configured();
                    Ready::Ok::<_, &'static str>(())
                },
                _t: PhantomData,
            }),
        }
    }

    /// Add new service to the server.
    pub fn bind<U, N: AsRef<str>>(&mut self, name: N, addr: U) -> io::Result<&mut Self>
    where
        U: net::ToSocketAddrs,
    {
        let sockets = bind_addr(addr, self.backlog)?;

        for lst in sockets {
            self.listen(name.as_ref(), lst);
        }

        Ok(self)
    }

    /// Add new service to the server.
    pub fn listen<N: AsRef<str>>(
        &mut self,
        name: N,
        lst: net::TcpListener,
    ) -> &mut Self {
        if !self.applied {
            self.apply = Box::new(ConfigWrapper {
                f: |_| {
                    not_configured();
                    Ready::Ok::<_, &'static str>(())
                },
                _t: PhantomData,
            });
        }
        self.services.push((name.as_ref().to_string(), lst));
        self
    }

    /// Register async service configuration function.
    ///
    /// This function get called during worker runtime configuration stage.
    /// It get executed in the worker thread.
    pub fn on_worker_start<F, R, E>(&mut self, f: F) -> io::Result<()>
    where
        F: Fn(ServiceRuntime) -> R + Send + Clone + 'static,
        R: Future<Output = Result<(), E>> + 'static,
        E: fmt::Display + 'static,
    {
        self.applied = true;
        self.apply = Box::new(ConfigWrapper { f, _t: PhantomData });
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

    fn create(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(Token, BoxedServerService)>, ()>>>>
    {
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
            for f in mem::take(&mut rt.0.borrow_mut().onstart).into_iter() {
                f.await;
            }
            let mut res = vec![];
            for token in tokens {
                if let Some(srv) = services.remove(&token) {
                    let newserv = srv.new_service(());
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
                        Box::new(StreamService::new(
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

    fn configure(
        &self,
        rt: ServiceRuntime,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>>>>;
}

pub(super) struct ConfigWrapper<F, R, E> {
    pub(super) f: F,
    pub(super) _t: PhantomData<(R, E)>,
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
            _t: PhantomData,
        })
    }

    fn configure(
        &self,
        rt: ServiceRuntime,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>>>> {
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
    services: HashMap<Token, BoxedNewService>,
    onstart: Vec<Pin<Box<dyn Future<Output = ()>>>>,
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
        T: service::ServiceFactory<Io, Config = ()> + 'static,
        T::Future: 'static,
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
        T: service::ServiceFactory<Io, Config = ()> + 'static,
        T::Future: 'static,
        T::Service: 'static,
        T::InitError: fmt::Debug,
    {
        let mut inner = self.0.borrow_mut();
        if let Some(token) = inner.names.get(name) {
            let token = *token;
            inner.services.insert(
                token,
                Box::new(ServiceFactory {
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

type BoxedNewService = Box<
    dyn service::ServiceFactory<
        (Option<CounterGuard>, ServerMessage),
        Response = (),
        Error = (),
        InitError = (),
        Config = (),
        Service = BoxedServerService,
        Future = Pin<Box<dyn Future<Output = Result<BoxedServerService, ()>>>>,
    >,
>;

struct ServiceFactory<T> {
    inner: T,
    pool: PoolId,
}

impl<T> service::ServiceFactory<(Option<CounterGuard>, ServerMessage)>
    for ServiceFactory<T>
where
    T: service::ServiceFactory<Io, Config = ()>,
    T::Future: 'static,
    T::Service: 'static,
    T::Error: 'static,
    T::InitError: fmt::Debug + 'static,
{
    type Response = ();
    type Error = ();
    type InitError = ();
    type Config = ();
    type Service = BoxedServerService;
    type Future = Pin<Box<dyn Future<Output = Result<BoxedServerService, ()>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let pool = self.pool;
        let fut = self.inner.new_service(());
        Box::pin(async move {
            match fut.await {
                Ok(s) => Ok(Box::new(StreamService::new(s, pool)) as BoxedServerService),
                Err(e) => {
                    error!("Cannot construct service: {:?}", e);
                    Err(())
                }
            }
        })
    }
}
