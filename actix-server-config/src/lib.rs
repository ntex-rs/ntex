use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;

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

#[derive(Copy, Clone, Debug)]
pub enum Protocol {
    Unknown,
    Http10,
    Http11,
    Http2,
    Proto1,
    Proto2,
    Proto3,
    Proto4,
    Proto5,
    Proto6,
}

pub struct Io<T, P = ()> {
    io: T,
    proto: Protocol,
    params: P,
}

impl<T> Io<T, ()> {
    pub fn new(io: T) -> Self {
        Self {
            io,
            proto: Protocol::Unknown,
            params: (),
        }
    }
}

impl<T, P> Io<T, P> {
    pub fn from_parts(io: T, params: P, proto: Protocol) -> Self {
        Self { io, params, proto }
    }

    pub fn into_parts(self) -> (T, P, Protocol) {
        (self.io, self.params, self.proto)
    }

    pub fn io(&self) -> &T {
        &self.io
    }

    pub fn io_mut(&mut self) -> &mut T {
        &mut self.io
    }

    pub fn protocol(&self) -> Protocol {
        self.proto
    }

    /// Maps an Io<_, P> to Io<_, U> by applying a function to a contained value.
    pub fn map<U, F>(self, op: F) -> Io<T, U>
    where
        F: FnOnce(P) -> U,
    {
        Io {
            io: self.io,
            proto: self.proto,
            params: op(self.params),
        }
    }
}
