use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;

use either::Either;

/// Connect request
pub trait Address {
    /// Host name of the request
    fn host(&self) -> &str;

    /// Port of the request
    fn port(&self) -> Option<u16>;
}

impl Address for String {
    fn host(&self) -> &str {
        &self
    }

    fn port(&self) -> Option<u16> {
        None
    }
}

impl Address for &'static str {
    fn host(&self) -> &str {
        self
    }

    fn port(&self) -> Option<u16> {
        None
    }
}

/// Connect request
#[derive(Eq, PartialEq, Debug, Hash)]
pub struct Connect<T> {
    pub(crate) req: T,
    pub(crate) port: u16,
    pub(crate) addr: Option<Either<SocketAddr, VecDeque<SocketAddr>>>,
}

impl<T: Address> Connect<T> {
    /// Create `Connect` instance by spliting the string by ':' and convert the second part to u16
    pub fn new(req: T) -> Connect<T> {
        let (_, port) = parse(req.host());
        Connect {
            req,
            port: port.unwrap_or(0),
            addr: None,
        }
    }

    /// Create new `Connect` instance from host and address. Connector skips name resolution stage for such connect messages.
    pub fn with(req: T, addr: SocketAddr) -> Connect<T> {
        Connect {
            req,
            port: 0,
            addr: Some(Either::Left(addr)),
        }
    }

    /// Use port if address does not provide one.
    ///
    /// By default it set to 0
    pub fn set_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Use address.
    pub fn set_addr(mut self, addr: Option<SocketAddr>) -> Self {
        if let Some(addr) = addr {
            self.addr = Some(Either::Left(addr));
        }
        self
    }

    /// Host name
    pub fn host(&self) -> &str {
        self.req.host()
    }

    /// Port of the request
    pub fn port(&self) -> u16 {
        self.req.port().unwrap_or(self.port)
    }
}

impl<T: Address> From<T> for Connect<T> {
    fn from(addr: T) -> Self {
        Connect::new(addr)
    }
}

impl<T: Address> fmt::Display for Connect<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.host(), self.port())
    }
}

fn parse(host: &str) -> (&str, Option<u16>) {
    let mut parts_iter = host.splitn(2, ':');
    if let Some(host) = parts_iter.next() {
        let port_str = parts_iter.next().unwrap_or("");
        if let Ok(port) = port_str.parse::<u16>() {
            (host, Some(port))
        } else {
            (host, None)
        }
    } else {
        (host, None)
    }
}

pub struct Connection<T, U> {
    io: U,
    req: T,
}

impl<T, U> Connection<T, U> {
    pub fn new(io: U, req: T) -> Self {
        Self { io, req }
    }
}

impl<T, U> Connection<T, U> {
    /// Reconstruct from a parts.
    pub fn from_parts(io: U, req: T) -> Self {
        Self { io, req }
    }

    /// Deconstruct into a parts.
    pub fn into_parts(self) -> (U, T) {
        (self.io, self.req)
    }

    /// Replace inclosed object, return new Stream and old object
    pub fn replace<Y>(self, io: Y) -> (U, Connection<T, Y>) {
        (self.io, Connection { io, req: self.req })
    }

    /// Returns a shared reference to the underlying stream.
    pub fn get_ref(&self) -> &U {
        &self.io
    }

    /// Returns a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut U {
        &mut self.io
    }
}

impl<T: Address, U> Connection<T, U> {
    /// Get request
    pub fn host(&self) -> &str {
        &self.req.host()
    }
}

impl<T, U> std::ops::Deref for Connection<T, U> {
    type Target = U;

    fn deref(&self) -> &U {
        &self.io
    }
}

impl<T, U> std::ops::DerefMut for Connection<T, U> {
    fn deref_mut(&mut self) -> &mut U {
        &mut self.io
    }
}

impl<T, U: fmt::Debug> fmt::Debug for Connection<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Stream {{{:?}}}", self.io)
    }
}
