use std::cell::Cell;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::rc::Rc;

use actix_service::{FnService, IntoService, NewService};
use futures::future::{ok, FutureResult, IntoFuture};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    addr: SocketAddr,
    secure: Rc<Cell<bool>>,
}

impl ServerConfig {
    pub fn new(addr: SocketAddr) -> Self {
        ServerConfig {
            addr,
            secure: Rc::new(Cell::new(false)),
        }
    }

    /// Returns the address of the local half of this TCP server socket
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Returns true if connection is secure (tls enabled)
    pub fn secure(&self) -> bool {
        self.secure.as_ref().get()
    }

    /// Set secure flag
    pub fn set_secure(&self) {
        self.secure.as_ref().set(true)
    }
}

pub fn server_fn<F, U, Req, Out>(f: F) -> impl NewService<Req, ServerConfig>
where
    F: Fn(&ServerConfig) -> U + Clone + 'static,
    U: FnMut(Req) -> Out + Clone + 'static,
    Out: IntoFuture,
{
    ServerFnNewService { f, _t: PhantomData }
}

struct ServerFnNewService<F, U, Req, Out>
where
    F: Fn(&ServerConfig) -> U + Clone + 'static,
    U: FnMut(Req) -> Out + Clone + 'static,
    Out: IntoFuture,
{
    f: F,
    _t: PhantomData<(U, Req, Out)>,
}

impl<F, U, Req, Out> NewService<Req, ServerConfig> for ServerFnNewService<F, U, Req, Out>
where
    F: Fn(&ServerConfig) -> U + Clone + 'static,
    U: FnMut(Req) -> Out + Clone + 'static,
    Out: IntoFuture,
{
    type Response = Out::Item;
    type Error = Out::Error;
    type Service = FnService<U, Req, Out>;

    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, cfg: &ServerConfig) -> Self::Future {
        ok((self.f)(cfg).into_service())
    }
}

impl<F, U, Req, Out> Clone for ServerFnNewService<F, U, Req, Out>
where
    F: Fn(&ServerConfig) -> U + Clone + 'static,
    U: FnMut(Req) -> Out + Clone,
    Out: IntoFuture,
{
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}
