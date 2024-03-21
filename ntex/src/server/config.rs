use std::{cell::Cell, cell::RefCell, fmt, future::Future, io, marker, net, rc::Rc};

use crate::io::Io;
use crate::service::{self, boxed};
use crate::util::{BoxFuture, HashMap, PoolId, Ready};

use super::service::{Factory, ServerMessage, StreamService};
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

pub(super) struct ServiceConfigInner {
    token: Token,
    apply: Box<dyn OnWorkerStart + Send>,
    sockets: Vec<(Token, String, Listener, &'static str)>,
    backlog: i32,
}

impl ServiceConfig {
    pub(super) fn new(token: Token, backlog: i32) -> Self {
        ServiceConfig(Rc::new(RefCell::new(ServiceConfigInner {
            token,
            backlog,
            sockets: Vec::new(),
            apply: OnWorkerStartWrapper::create(|_| {
                not_configured();
                Ready::Ok::<_, &str>(())
            }),
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
        let inner = self.0.borrow_mut();
        inner.sockets.push((
            inner.token.next(),
            name.as_ref().to_string(),
            Listener::Tcp(lst),
            "",
        ));
        self
    }

    /// Set io tag for configured service.
    pub fn set_tag<N: AsRef<str>>(&self, name: N, tag: &'static str) -> &Self {
        let mut inner = self.0.borrow_mut();
        for svc in &mut inner.sockets {
            if svc.1 == name.as_ref() {
                svc.3 = tag;
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
        E: fmt::Display,
    {
        self.0.borrow_mut().apply = OnWorkerStartWrapper::create(f);
        self
    }

    pub(super) fn into_factory(self) -> (Token, Vec<(Token, String, Listener)>, Factory) {
        todo!()
    }
}

// pub(super) struct ConfiguredService {
//     rt: Box<dyn ServiceRuntimeConfiguration + Send>,
//     names: HashMap<Token, (String, net::SocketAddr)>,
//     topics: HashMap<String, (Token, &'static str)>,
//     services: Vec<Token>,
// }

// impl ConfiguredService {
//     pub(super) fn new(rt: Box<dyn ServiceRuntimeConfiguration + Send>) -> Self {
//         ConfiguredService {
//             rt,
//             names: HashMap::default(),
//             topics: HashMap::default(),
//             services: Vec::new(),
//         }
//     }

//     pub(super) fn stream(
//         &mut self,
//         token: Token,
//         name: String,
//         addr: net::SocketAddr,
//         tag: &'static str,
//     ) {
//         self.names.insert(token, (name.clone(), addr));
//         self.topics.insert(name, (token, tag));
//         self.services.push(token);
//     }
// }

// fn create(&self) -> BoxFuture<'static, Result<Vec<(Token, BoxedServerService)>, ()>> {
//     // configure services
//     let rt = ServiceRuntime::new(self.topics.clone());
//     let cfg_fut = self.rt.configure(ServiceRuntime(rt.0.clone()));
//     let mut names = self.names.clone();
//     let tokens = self.services.clone();

//     // construct services
//     Box::pin(async move {
//         cfg_fut.await?;
//         rt.validate();

//         let mut services = mem::take(&mut rt.0.borrow_mut().services);
//         // TODO: Proper error handling here
//         let onstart = mem::take(&mut rt.0.borrow_mut().onstart);
//         for f in onstart.into_iter() {
//             f.await;
//         }
//         let mut res = vec![];
//         for token in tokens {
//             if let Some(srv) = services.remove(&token) {
//                 let newserv = srv.create(());
//                 match newserv.await {
//                     Ok(serv) => {
//                         res.push((token, serv));
//                     }
//                     Err(_) => {
//                         log::error!("Cannot construct service");
//                         return Err(());
//                     }
//                 }
//             } else {
//                 let name = names.remove(&token).unwrap().0;
//                 res.push((
//                     token,
//                     boxed::service(StreamService::new(
//                         service::fn_service(move |_: Io| {
//                             log::error!("Service {:?} is not configured", name);
//                             Ready::<_, ()>::Ok(())
//                         }),
//                         "UNKNOWN",
//                         PoolId::P0,
//                     )),
//                 ));
//             };
//         }
//         Ok(res)
//     })
// }

fn not_configured() {
    log::error!("Service is not configured");
}

pub struct ServiceRuntime(Rc<RefCell<ServiceRuntimeInner>>);

struct ServiceRuntimeInner {
    names: HashMap<String, (Token, &'static str)>,
    services: HashMap<Token, BoxServiceFactory>,
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
    fn new(names: HashMap<String, (Token, &'static str)>) -> Self {
        ServiceRuntime(Rc::new(RefCell::new(ServiceRuntimeInner {
            names,
            services: HashMap::default(),
        })))
    }

    fn validate(&self) {
        let inner = self.0.as_ref().borrow();
        for (name, item) in &inner.names {
            if !inner.services.contains_key(&item.0) {
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
        if let Some((token, tag)) = inner.names.get(name) {
            let token = *token;
            let tag = *tag;
            inner.services.insert(
                token,
                boxed::factory(ServiceFactory {
                    tag,
                    pool,
                    inner: service.into_factory(),
                }),
            );
        } else {
            panic!("Unknown service: {:?}", name);
        }
    }
}

type BoxServiceFactory = service::boxed::BoxServiceFactory<(), ServerMessage, (), (), ()>;

struct ServiceFactory<T> {
    inner: T,
    tag: &'static str,
    pool: PoolId,
}

impl<T> service::ServiceFactory<ServerMessage> for ServiceFactory<T>
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

    async fn create(&self, _: ()) -> Result<BoxedServerService, ()> {
        let tag = self.tag;
        let pool = self.pool;

        match self.inner.create(()).await {
            Ok(s) => Ok(boxed::service(StreamService::new(s, tag, pool))),
            Err(e) => {
                log::error!("Cannot construct service: {:?}", e);
                Err(())
            }
        }
    }
}

trait OnWorkerStart {
    fn clone(&self) -> Box<dyn OnWorkerStart + Send>;

    fn run(&self, rt: ServiceRuntime) -> BoxFuture<'static, Result<(), ()>>;
}

struct OnWorkerStartWrapper<F, R, E> {
    pub(super) f: F,
    pub(super) _t: marker::PhantomData<(R, E)>,
}

unsafe impl<F, R, E> Send for OnWorkerStartWrapper<F, R, E> where F: Send {}

impl<F, R, E> OnWorkerStartWrapper<F, R, E>
where
    F: Fn(ServiceRuntime) -> R + Send + Clone + 'static,
    R: Future<Output = Result<(), E>> + 'static,
    E: fmt::Display,
{
    pub(super) fn create(f: F) -> Box<dyn OnWorkerStart + Send> {
        Box::new(Self {
            f,
            _t: marker::PhantomData,
        })
    }
}

impl<F, R, E> OnWorkerStart for OnWorkerStartWrapper<F, R, E>
where
    F: Fn(ServiceRuntime) -> R + Send + Clone + 'static,
    R: Future<Output = Result<(), E>> + 'static,
    E: fmt::Display,
{
    fn clone(&self) -> Box<dyn OnWorkerStart + Send> {
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
