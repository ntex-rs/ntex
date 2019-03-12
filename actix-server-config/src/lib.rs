use std::cell::Cell;
use std::fmt;
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

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
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
    /// Reconstruct from a parts.
    pub fn from_parts(io: T, params: P, proto: Protocol) -> Self {
        Self { io, params, proto }
    }

    /// Deconstruct into a parts.
    pub fn into_parts(self) -> (T, P, Protocol) {
        (self.io, self.params, self.proto)
    }

    /// Returns a shared reference to the underlying stream.
    pub fn get_ref(&self) -> &T {
        &self.io
    }

    /// Returns a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.io
    }

    /// Get selected protocol
    pub fn protocol(&self) -> Protocol {
        self.proto
    }

    /// Return new Io object with new parameter.
    pub fn set<U>(self, params: U) -> Io<T, U> {
        Io {
            io: self.io,
            proto: self.proto,
            params: params,
        }
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

impl<T, P> std::ops::Deref for Io<T, P> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.io
    }
}

impl<T, P> std::ops::DerefMut for Io<T, P> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.io
    }
}

impl<T: fmt::Debug, P> fmt::Debug for Io<T, P> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Io {{{:?}}}", self.io)
    }
}
