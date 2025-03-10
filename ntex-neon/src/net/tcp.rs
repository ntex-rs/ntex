use std::{future::Future, io, net::SocketAddr};

use socket2::Socket as Socket2;

use crate::{impl_raw_fd, net::Socket};

/// A TCP stream between a local and a remote socket.
///
/// A TCP stream can either be created by connecting to an endpoint, via the
/// `connect` method, or by accepting a connection from a listener.
#[derive(Debug)]
pub struct TcpStream {
    inner: Socket,
}

impl TcpStream {
    /// Creates new TcpStream from a std::net::TcpStream.
    pub fn from_std(stream: std::net::TcpStream) -> io::Result<Self> {
        Ok(Self {
            inner: Socket::from_socket2(Socket2::from(stream))?,
        })
    }

    /// Creates new TcpStream from a std::net::TcpStream.
    pub fn from_socket(inner: Socket) -> Self {
        Self { inner }
    }

    /// Close the socket.
    pub fn close(self) -> impl Future<Output = io::Result<()>> {
        self.inner.close()
    }

    /// Returns the socket address of the remote peer of this TCP connection.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner
            .peer_addr()
            .map(|addr| addr.as_socket().expect("should be SocketAddr"))
    }

    /// Returns the socket address of the local half of this TCP connection.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner
            .local_addr()
            .map(|addr| addr.as_socket().expect("should be SocketAddr"))
    }
}

impl_raw_fd!(TcpStream, socket2::Socket, inner, socket);
