use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::{fmt, io, net, time};

use tokio_io::{AsyncRead, AsyncWrite};
use tokio_tcp::TcpStream;

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
            params,
            io: self.io,
            proto: self.proto,
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

/// Low-level io stream operations
pub trait IoStream: AsyncRead + AsyncWrite {
    /// Returns the socket address of the remote peer of this TCP connection.
    fn peer_addr(&self) -> Option<SocketAddr> {
        None
    }

    /// Sets the value of the TCP_NODELAY option on this socket.
    fn set_nodelay(&mut self, nodelay: bool) -> io::Result<()>;

    fn set_linger(&mut self, dur: Option<time::Duration>) -> io::Result<()>;

    fn set_keepalive(&mut self, dur: Option<time::Duration>) -> io::Result<()>;
}

impl IoStream for TcpStream {
    #[inline]
    fn peer_addr(&self) -> Option<net::SocketAddr> {
        TcpStream::peer_addr(self).ok()
    }

    #[inline]
    fn set_nodelay(&mut self, nodelay: bool) -> io::Result<()> {
        TcpStream::set_nodelay(self, nodelay)
    }

    #[inline]
    fn set_linger(&mut self, dur: Option<time::Duration>) -> io::Result<()> {
        TcpStream::set_linger(self, dur)
    }

    #[inline]
    fn set_keepalive(&mut self, dur: Option<time::Duration>) -> io::Result<()> {
        TcpStream::set_keepalive(self, dur)
    }
}

#[cfg(any(feature = "ssl"))]
impl<T: IoStream> IoStream for tokio_openssl::SslStream<T> {
    #[inline]
    fn peer_addr(&self) -> Option<net::SocketAddr> {
        self.get_ref().get_ref().peer_addr()
    }

    #[inline]
    fn set_nodelay(&mut self, nodelay: bool) -> io::Result<()> {
        self.get_mut().get_mut().set_nodelay(nodelay)
    }

    #[inline]
    fn set_linger(&mut self, dur: Option<time::Duration>) -> io::Result<()> {
        self.get_mut().get_mut().set_linger(dur)
    }

    #[inline]
    fn set_keepalive(&mut self, dur: Option<time::Duration>) -> io::Result<()> {
        self.get_mut().get_mut().set_keepalive(dur)
    }
}

#[cfg(any(feature = "rust-tls"))]
impl<T: IoStream> IoStream for tokio_rustls::server::TlsStream<T> {
    #[inline]
    fn peer_addr(&self) -> Option<net::SocketAddr> {
        self.get_ref().0.peer_addr()
    }

    #[inline]
    fn set_nodelay(&mut self, nodelay: bool) -> io::Result<()> {
        self.get_mut().0.set_nodelay(nodelay)
    }

    #[inline]
    fn set_linger(&mut self, dur: Option<time::Duration>) -> io::Result<()> {
        self.get_mut().0.set_linger(dur)
    }

    #[inline]
    fn set_keepalive(&mut self, dur: Option<time::Duration>) -> io::Result<()> {
        self.get_mut().0.set_keepalive(dur)
    }
}

#[cfg(all(unix, feature = "uds"))]
impl IoStream for tokio_uds::UnixStream {
    #[inline]
    fn peer_addr(&self) -> Option<net::SocketAddr> {
        None
    }

    #[inline]
    fn set_nodelay(&mut self, _: bool) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn set_linger(&mut self, _: Option<time::Duration>) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn set_keepalive(&mut self, _: Option<time::Duration>) -> io::Result<()> {
        Ok(())
    }
}
