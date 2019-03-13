use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;

use either::Either;

use crate::error::ConnectError;

/// Connect request
#[derive(Eq, PartialEq, Debug, Hash)]
pub enum Connect {
    /// Host name
    Host { host: String, port: u16 },
    /// Host name with address of this host
    Addr {
        host: String,
        addr: Either<SocketAddr, VecDeque<SocketAddr>>,
    },
}

impl Connect {
    /// Create new `Connect` instance.
    pub fn new<T: AsRef<str>>(host: T, port: u16) -> Connect {
        Connect::Host {
            host: host.as_ref().to_owned(),
            port,
        }
    }

    /// Create `Connect` instance by spliting the string by ':' and convert the second part to u16
    pub fn with<T: AsRef<str>>(host: T) -> Result<Connect, ConnectError> {
        let mut parts_iter = host.as_ref().splitn(2, ':');
        let host = parts_iter.next().ok_or(ConnectError::InvalidInput)?;
        let port_str = parts_iter.next().unwrap_or("");
        let port = port_str
            .parse::<u16>()
            .map_err(|_| ConnectError::InvalidInput)?;
        Ok(Connect::Host {
            host: host.to_owned(),
            port,
        })
    }

    /// Create new `Connect` instance from host and address. Connector skips name resolution stage for such connect messages.
    pub fn with_address<T: Into<String>>(host: T, addr: SocketAddr) -> Connect {
        Connect::Addr {
            addr: Either::Left(addr),
            host: host.into(),
        }
    }

    /// Host name
    fn host(&self) -> &str {
        match self {
            Connect::Host { ref host, .. } => host,
            Connect::Addr { ref host, .. } => host,
        }
    }
}

impl fmt::Display for Connect {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.host(), 0)
    }
}

pub struct Stream<T, P = ()> {
    io: T,
    host: String,
    params: P,
}

impl<T> Stream<T, ()> {
    pub fn new(io: T, host: String) -> Self {
        Self {
            io,
            host,
            params: (),
        }
    }
}

impl<T, P> Stream<T, P> {
    /// Reconstruct from a parts.
    pub fn from_parts(io: T, host: String, params: P) -> Self {
        Self { io, params, host }
    }

    /// Deconstruct into a parts.
    pub fn into_parts(self) -> (T, String, P) {
        (self.io, self.host, self.params)
    }

    /// Replace inclosed object, return new Stream and old object
    pub fn replace<U>(self, io: U) -> (T, Stream<U, P>) {
        (
            self.io,
            Stream {
                io,
                host: self.host,
                params: self.params,
            },
        )
    }

    /// Returns a shared reference to the underlying stream.
    pub fn get_ref(&self) -> &T {
        &self.io
    }

    /// Returns a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.io
    }

    /// Get host name
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Return new Io object with new parameter.
    pub fn set<U>(self, params: U) -> Stream<T, U> {
        Stream {
            io: self.io,
            host: self.host,
            params: params,
        }
    }

    /// Maps an Io<_, P> to Io<_, U> by applying a function to a contained value.
    pub fn map<U, F>(self, op: F) -> Stream<T, U>
    where
        F: FnOnce(P) -> U,
    {
        Stream {
            io: self.io,
            host: self.host,
            params: op(self.params),
        }
    }
}

impl<T, P> std::ops::Deref for Stream<T, P> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.io
    }
}

impl<T, P> std::ops::DerefMut for Stream<T, P> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.io
    }
}

impl<T: fmt::Debug, P> fmt::Debug for Stream<T, P> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Stream {{{:?}}}", self.io)
    }
}
