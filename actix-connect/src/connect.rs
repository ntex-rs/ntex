use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;

use either::Either;

use crate::error::ConnectError;

/// Connect request
pub trait Address {
    /// Host name of the request
    fn host(&self) -> &str;

    /// Port of the request
    fn port(&self) -> u16;
}

impl Address for (String, u16) {
    fn host(&self) -> &str {
        &self.0
    }

    fn port(&self) -> u16 {
        self.1
    }
}

impl Address for (&'static str, u16) {
    fn host(&self) -> &str {
        self.0
    }

    fn port(&self) -> u16 {
        self.1
    }
}

/// Connect request
#[derive(Eq, PartialEq, Debug, Hash)]
pub struct Connect<T> {
    pub(crate) req: T,
    pub(crate) addr: Option<Either<SocketAddr, VecDeque<SocketAddr>>>,
}

impl Connect<(&'static str, u16)> {
    /// Create new `Connect` instance.
    pub fn new(host: &'static str, port: u16) -> Connect<(&'static str, u16)> {
        Connect {
            req: (host, port),
            addr: None,
        }
    }
}

impl Connect<()> {
    /// Create new `Connect` instance.
    pub fn from_string(host: String, port: u16) -> Connect<(String, u16)> {
        Connect {
            req: (host, port),
            addr: None,
        }
    }

    /// Create new `Connect` instance.
    pub fn from_static(host: &'static str, port: u16) -> Connect<(&'static str, u16)> {
        Connect {
            req: (host, port),
            addr: None,
        }
    }

    /// Create `Connect` instance by spliting the string by ':' and convert the second part to u16
    pub fn from_str<T: AsRef<str>>(host: T) -> Result<Connect<(String, u16)>, ConnectError> {
        let mut parts_iter = host.as_ref().splitn(2, ':');
        let host = parts_iter.next().ok_or(ConnectError::InvalidInput)?;
        let port_str = parts_iter.next().unwrap_or("");
        let port = port_str
            .parse::<u16>()
            .map_err(|_| ConnectError::InvalidInput)?;
        Ok(Connect {
            req: (host.to_owned(), port),
            addr: None,
        })
    }
}

impl<T: Address> Connect<T> {
    /// Create new `Connect` instance.
    pub fn with(req: T) -> Connect<T> {
        Connect { req, addr: None }
    }

    /// Create new `Connect` instance from host and address. Connector skips name resolution stage for such connect messages.
    pub fn with_address(req: T, addr: SocketAddr) -> Connect<T> {
        Connect {
            req,
            addr: Some(Either::Left(addr)),
        }
    }

    /// Host name
    pub fn host(&self) -> &str {
        self.req.host()
    }

    /// Port of the request
    pub fn port(&self) -> u16 {
        self.req.port()
    }
}

impl<T: Address> fmt::Display for Connect<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.host(), self.port())
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
