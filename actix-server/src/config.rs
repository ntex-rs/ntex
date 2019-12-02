use std::collections::HashMap;
use std::{fmt, io, net};

use actix_rt::net::TcpStream;
use actix_service as actix;
use futures::future::{Future, FutureExt, LocalBoxFuture};
use log::error;

use super::builder::bind_addr;
use super::service::{
    BoxedServerService, InternalServiceFactory, ServerMessage, StreamService,
};
use super::Token;
use crate::counter::CounterGuard;

pub struct ServiceConfig {
    pub(crate) services: Vec<(String, net::TcpListener)>,
    pub(crate) apply: Option<Box<dyn ServiceRuntimeConfiguration>>,
    pub(crate) threads: usize,
    pub(crate) backlog: i32,
}

impl ServiceConfig {
    pub(super) fn new(threads: usize, backlog: i32) -> ServiceConfig {
        ServiceConfig {
            threads,
            backlog,
            services: Vec::new(),
            apply: None,
        }
    }

    /// Set number of workers to start.
    ///
    /// By default server uses number of available logical cpu as workers
    /// count.
    pub fn workers(&mut self, num: usize) {
        self.threads = num;
    }

    /// Add new service to server
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

    /// Add new service to server
    pub fn listen<N: AsRef<str>>(&mut self, name: N, lst: net::TcpListener) -> &mut Self {
        if self.apply.is_none() {
            self.apply = Some(Box::new(not_configured));
        }
        self.services.push((name.as_ref().to_string(), lst));
        self
    }

    /// Register service configuration function. This function get called
    /// during worker runtime configuration. It get executed in worker thread.
    pub fn apply<F>(&mut self, f: F) -> io::Result<()>
    where
        F: Fn(&mut ServiceRuntime) + Send + Clone + 'static,
    {
        self.apply = Some(Box::new(f));
        Ok(())
    }
}

pub(super) struct ConfiguredService {
    rt: Box<dyn ServiceRuntimeConfiguration>,
    names: HashMap<Token, (String, net::SocketAddr)>,
    services: HashMap<String, Token>,
}

impl ConfiguredService {
    pub(super) fn new(rt: Box<dyn ServiceRuntimeConfiguration>) -> Self {
        ConfiguredService {
            rt,
            names: HashMap::new(),
            services: HashMap::new(),
        }
    }

    pub(super) fn stream(&mut self, token: Token, name: String, addr: net::SocketAddr) {
        self.names.insert(token, (name.clone(), addr));
        self.services.insert(name, token);
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
            services: self.services.clone(),
        })
    }

    fn create(&self) -> LocalBoxFuture<'static, Result<Vec<(Token, BoxedServerService)>, ()>> {
        // configure services
        let mut rt = ServiceRuntime::new(self.services.clone());
        self.rt.configure(&mut rt);
        rt.validate();

        // construct services
        async move {
            let services = rt.services;
            // TODO: Proper error handling here
            for f in rt.onstart.into_iter() {
                f.await;
            }
            let mut res = vec![];
            for (token, ns) in services.into_iter() {
                let newserv = ns.new_service(&());
                match newserv.await {
                    Ok(serv) => {
                        res.push((token, serv));
                    }
                    Err(e) => {
                        error!("Can not construct service {:?}", e);
                        return Err(e);
                    }
                };
            }
            return Ok(res);
        }
            .boxed_local()
    }
}

pub(super) trait ServiceRuntimeConfiguration: Send {
    fn clone(&self) -> Box<dyn ServiceRuntimeConfiguration>;

    fn configure(&self, rt: &mut ServiceRuntime);
}

impl<F> ServiceRuntimeConfiguration for F
where
    F: Fn(&mut ServiceRuntime) + Send + Clone + 'static,
{
    fn clone(&self) -> Box<dyn ServiceRuntimeConfiguration> {
        Box::new(self.clone())
    }

    fn configure(&self, rt: &mut ServiceRuntime) {
        (self)(rt)
    }
}

fn not_configured(_: &mut ServiceRuntime) {
    error!("Service is not configured");
}

pub struct ServiceRuntime {
    names: HashMap<String, Token>,
    services: HashMap<Token, BoxedNewService>,
    onstart: Vec<LocalBoxFuture<'static, ()>>,
}

impl ServiceRuntime {
    fn new(names: HashMap<String, Token>) -> Self {
        ServiceRuntime {
            names,
            services: HashMap::new(),
            onstart: Vec::new(),
        }
    }

    fn validate(&self) {
        for (name, token) in &self.names {
            if !self.services.contains_key(&token) {
                error!("Service {:?} is not configured", name);
            }
        }
    }

    /// Register service.
    ///
    /// Name of the service must be registered during configuration stage with
    /// *ServiceConfig::bind()* or *ServiceConfig::listen()* methods.
    pub fn service<T, F>(&mut self, name: &str, service: F)
    where
        F: actix::IntoServiceFactory<T>,
        T: actix::ServiceFactory<Config = (), Request = TcpStream> + 'static,
        T::Future: 'static,
        T::Service: 'static,
        T::InitError: fmt::Debug,
    {
        // let name = name.to_owned();
        if let Some(token) = self.names.get(name) {
            self.services.insert(
                token.clone(),
                Box::new(ServiceFactory {
                    inner: service.into_factory(),
                }),
            );
        } else {
            panic!("Unknown service: {:?}", name);
        }
    }

    /// Execute future before services initialization.
    pub fn on_start<F>(&mut self, fut: F)
    where
        F: Future<Output = ()> + 'static,
    {
        self.onstart.push(fut.boxed_local())
    }
}

type BoxedNewService = Box<
    dyn actix::ServiceFactory<
        Request = (Option<CounterGuard>, ServerMessage),
        Response = (),
        Error = (),
        InitError = (),
        Config = (),
        Service = BoxedServerService,
        Future = LocalBoxFuture<'static, Result<BoxedServerService, ()>>,
    >,
>;

struct ServiceFactory<T> {
    inner: T,
}

impl<T> actix::ServiceFactory for ServiceFactory<T>
where
    T: actix::ServiceFactory<Config = (), Request = TcpStream>,
    T::Future: 'static,
    T::Service: 'static,
    T::Error: 'static,
    T::InitError: fmt::Debug + 'static,
{
    type Request = (Option<CounterGuard>, ServerMessage);
    type Response = ();
    type Error = ();
    type InitError = ();
    type Config = ();
    type Service = BoxedServerService;
    type Future = LocalBoxFuture<'static, Result<BoxedServerService, ()>>;

    fn new_service(&self, cfg: &()) -> Self::Future {
        let fut = self.inner.new_service(cfg);
        async move {
            return match fut.await {
                Ok(s) => Ok(Box::new(StreamService::new(s)) as BoxedServerService),
                Err(e) => {
                    error!("Can not construct service: {:?}", e);
                    Err(())
                }
            };
        }
            .boxed_local()
    }
}
