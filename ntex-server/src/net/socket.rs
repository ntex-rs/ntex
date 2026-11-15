use std::{fmt, io, net};

use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

use super::Token;

/// Accepted socket stream.
#[derive(Debug)]
pub enum Stream {
    /// TCP stream.
    Tcp(net::TcpStream),
    /// Unix domain socket stream.
    #[cfg(unix)]
    Uds(std::os::unix::net::UnixStream),
}

impl Stream {
    /// Converts the stream, `nodelay` tells `TCP_NODELAY` is already set.
    pub(crate) fn convert(self, nodelay: bool, cfg: SharedCfg) -> Result<Io, io::Error> {
        match self {
            Stream::Tcp(stream) => {
                if !nodelay {
                    stream.set_nodelay(true)?;
                }
                ntex_net::from_tcp_stream(stream, cfg)
            }
            #[cfg(unix)]
            Stream::Uds(stream) => ntex_net::from_unix_stream(stream, cfg),
        }
    }
}

/// Accepted connection dispatched to a worker.
#[derive(Debug)]
pub struct Connection {
    pub(crate) io: Stream,
    pub(crate) token: Token,
    /// `TCP_NODELAY` is inherited from the listener.
    pub(crate) nodelay: bool,
}

impl Connection {
    /// Returns the CPU that handled the connection's incoming packets.
    pub(crate) fn incoming_cpu(&self) -> Option<usize> {
        match self.io {
            Stream::Tcp(ref stream) => crate::affinity::incoming_cpu(stream),
            #[cfg(unix)]
            Stream::Uds(_) => None,
        }
    }
}

pub enum Listener {
    Tcp(net::TcpListener),
    #[cfg(unix)]
    Uds(std::os::unix::net::UnixListener),
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Listener::Tcp(ref lst) => write!(f, "{lst:?}"),
            #[cfg(unix)]
            Listener::Uds(ref lst) => write!(f, "{lst:?}"),
        }
    }
}

impl fmt::Display for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Listener::Tcp(ref lst) => write!(f, "{}", lst.local_addr().ok().unwrap()),
            #[cfg(unix)]
            Listener::Uds(ref lst) => {
                write!(f, "{:?}", lst.local_addr().ok().unwrap())
            }
        }
    }
}

pub(crate) enum SocketAddr {
    Tcp(net::SocketAddr),
    #[cfg(unix)]
    Uds(std::os::unix::net::SocketAddr),
}

impl fmt::Display for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SocketAddr::Tcp(ref addr) => write!(f, "{addr}"),
            #[cfg(unix)]
            SocketAddr::Uds(ref addr) => write!(f, "{addr:?}"),
        }
    }
}

impl fmt::Debug for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SocketAddr::Tcp(ref addr) => write!(f, "{addr:?}"),
            #[cfg(unix)]
            SocketAddr::Uds(ref addr) => write!(f, "{addr:?}"),
        }
    }
}

impl Listener {
    pub(super) fn from_tcp(lst: net::TcpListener) -> Self {
        let _ = lst.set_nonblocking(true);
        // accepted sockets inherit the option, so workers skip setting it
        if let Err(e) = socket2::SockRef::from(&lst).set_tcp_nodelay(true) {
            log::warn!("Cannot set TCP_NODELAY for {lst:?}: {e}");
        }
        Listener::Tcp(lst)
    }

    /// Returns `true` if accepted streams inherit `TCP_NODELAY`.
    pub(crate) fn nodelay(&self) -> bool {
        match self {
            Listener::Tcp(lst) => tcp_nodelay(lst),
            #[cfg(unix)]
            Listener::Uds(_) => false,
        }
    }

    #[cfg(unix)]
    pub(super) fn from_uds(lst: std::os::unix::net::UnixListener) -> Self {
        let _ = lst.set_nonblocking(true);
        Listener::Uds(lst)
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        match self {
            Listener::Tcp(lst) => SocketAddr::Tcp(lst.local_addr().unwrap()),
            #[cfg(unix)]
            Listener::Uds(lst) => SocketAddr::Uds(lst.local_addr().unwrap()),
        }
    }

    pub(crate) fn accept(&self) -> io::Result<Option<Stream>> {
        match *self {
            Listener::Tcp(ref lst) => lst.accept().map(|(stream, _)| Some(Stream::Tcp(stream))),
            #[cfg(unix)]
            Listener::Uds(ref lst) => lst.accept().map(|(stream, _)| Some(Stream::Uds(stream))),
        }
    }

    pub(crate) fn remove_source(&self) {
        match *self {
            Listener::Tcp(_) => (),
            #[cfg(unix)]
            Listener::Uds(ref lst) => {
                // cleanup file path
                if let Ok(addr) = lst.local_addr()
                    && let Some(path) = addr.as_pathname()
                {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

/// Returns `true` if streams accepted on `lst` inherit `TCP_NODELAY`.
///
/// Only Linux and Apple platforms are known to copy the option to accepted
/// sockets.
pub(crate) fn tcp_nodelay(lst: &net::TcpListener) -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple"
    )) && socket2::SockRef::from(lst).tcp_nodelay().unwrap_or(false)
}

#[cfg(unix)]
mod listener_impl {
    use std::os::fd::{AsFd, BorrowedFd};
    use std::os::unix::io::{AsRawFd, RawFd};

    impl AsFd for super::Listener {
        fn as_fd(&self) -> BorrowedFd<'_> {
            match *self {
                super::Listener::Tcp(ref lst) => lst.as_fd(),
                super::Listener::Uds(ref lst) => lst.as_fd(),
            }
        }
    }

    impl AsRawFd for super::Listener {
        fn as_raw_fd(&self) -> RawFd {
            match *self {
                super::Listener::Tcp(ref lst) => lst.as_raw_fd(),
                super::Listener::Uds(ref lst) => lst.as_raw_fd(),
            }
        }
    }
}

#[cfg(windows)]
mod listener_impl {
    use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket, RawSocket};

    impl AsSocket for super::Listener {
        fn as_socket(&self) -> BorrowedSocket<'_> {
            match *self {
                super::Listener::Tcp(ref lst) => lst.as_socket(),
            }
        }
    }

    impl AsRawSocket for super::Listener {
        fn as_raw_socket(&self) -> RawSocket {
            match *self {
                super::Listener::Tcp(ref lst) => lst.as_raw_socket(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_addr() {
        use socket2::{Domain, SockAddr, Socket, Type};

        let addr = SocketAddr::Tcp("127.0.0.1:8080".parse().unwrap());
        assert!(format!("{addr:?}").contains("127.0.0.1:8080"));
        assert_eq!(format!("{addr}"), "127.0.0.1:8080");

        let addr: net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket.set_reuse_address(true).unwrap();
        socket.bind(&SockAddr::from(addr)).unwrap();
        let lst = Listener::Tcp(net::TcpListener::from(socket));
        assert!(format!("{lst:?}").contains("TcpListener"));
        assert!(format!("{lst}").contains("127.0.0.1"));
    }

    /// STATEMENT: A server TCP listener enables `TCP_NODELAY` and accepted
    /// streams inherit it, so workers may skip setting the option.
    ///
    /// HOW:
    /// - Wrap a plain listener with `Listener::from_tcp`.
    /// - Check `Listener::nodelay` matches the platform support.
    /// - Accept a connection and read `TCP_NODELAY` on the accepted stream.
    #[test]
    fn listener_nodelay_is_inherited() {
        let tcp = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = tcp.local_addr().unwrap();
        let lst = Listener::from_tcp(tcp);
        let supported = cfg!(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple"
        ));
        assert_eq!(lst.nodelay(), supported);

        let _client = net::TcpStream::connect(addr).unwrap();
        let stream = loop {
            match lst.accept() {
                Ok(Some(Stream::Tcp(stream))) => break stream,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                res => panic!("unexpected accept result: {res:?}"),
            }
        };
        if supported {
            assert!(stream.nodelay().unwrap());
        }
    }

    #[test]
    #[cfg(unix)]
    fn uds() {
        use std::os::unix::net::UnixListener;

        let _ = std::fs::remove_file("/tmp/sock.xxxxx");
        if let Ok(lst) = UnixListener::bind("/tmp/sock.xxxxx") {
            let addr = lst.local_addr().expect("Couldn't get local address");
            let a = SocketAddr::Uds(addr);
            assert!(format!("{a:?}").contains("/tmp/sock.xxxxx"));
            assert!(format!("{a}").contains("/tmp/sock.xxxxx"));

            let lst = Listener::Uds(lst);
            assert!(format!("{lst:?}").contains("/tmp/sock.xxxxx"));
            assert!(format!("{lst}").contains("/tmp/sock.xxxxx"));
        }
    }
}
