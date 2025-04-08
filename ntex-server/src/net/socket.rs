use std::{fmt, io, net};

use ntex_net::{self as rt, Io};

use super::Token;

#[derive(Debug)]
pub enum Stream {
    Tcp(net::TcpStream),
    #[cfg(unix)]
    Uds(std::os::unix::net::UnixStream),
}

impl TryFrom<Stream> for Io {
    type Error = io::Error;

    fn try_from(sock: Stream) -> Result<Self, Self::Error> {
        match sock {
            Stream::Tcp(stream) => rt::from_tcp_stream(stream),
            #[cfg(unix)]
            Stream::Uds(stream) => rt::from_unix_stream(stream),
        }
    }
}

#[derive(Debug)]
pub struct Connection {
    pub(crate) io: Stream,
    pub(crate) token: Token,
}

pub enum Listener {
    Tcp(net::TcpListener),
    #[cfg(unix)]
    Uds(std::os::unix::net::UnixListener),
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Listener::Tcp(ref lst) => write!(f, "{:?}", lst),
            #[cfg(unix)]
            Listener::Uds(ref lst) => write!(f, "{:?}", lst),
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
            SocketAddr::Tcp(ref addr) => write!(f, "{}", addr),
            #[cfg(unix)]
            SocketAddr::Uds(ref addr) => write!(f, "{:?}", addr),
        }
    }
}

impl fmt::Debug for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SocketAddr::Tcp(ref addr) => write!(f, "{:?}", addr),
            #[cfg(unix)]
            SocketAddr::Uds(ref addr) => write!(f, "{:?}", addr),
        }
    }
}

impl Listener {
    pub(super) fn from_tcp(lst: net::TcpListener) -> Self {
        let _ = lst.set_nonblocking(true);
        Listener::Tcp(lst)
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
            Listener::Tcp(ref lst) => {
                lst.accept().map(|(stream, _)| Some(Stream::Tcp(stream)))
            }
            #[cfg(unix)]
            Listener::Uds(ref lst) => {
                lst.accept().map(|(stream, _)| Some(Stream::Uds(stream)))
            }
        }
    }

    pub(crate) fn remove_source(&self) {
        match *self {
            Listener::Tcp(_) => (),
            #[cfg(unix)]
            Listener::Uds(ref lst) => {
                // cleanup file path
                if let Ok(addr) = lst.local_addr() {
                    if let Some(path) = addr.as_pathname() {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
mod listener_impl {
    use super::*;
    use std::os::fd::{AsFd, BorrowedFd};
    use std::os::unix::io::{AsRawFd, RawFd};

    impl AsFd for Listener {
        fn as_fd(&self) -> BorrowedFd<'_> {
            match *self {
                Listener::Tcp(ref lst) => lst.as_fd(),
                Listener::Uds(ref lst) => lst.as_fd(),
            }
        }
    }

    impl AsRawFd for Listener {
        fn as_raw_fd(&self) -> RawFd {
            match *self {
                Listener::Tcp(ref lst) => lst.as_raw_fd(),
                Listener::Uds(ref lst) => lst.as_raw_fd(),
            }
        }
    }
}

#[cfg(windows)]
mod listener_impl {
    use super::*;
    use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket, RawSocket};

    impl AsSocket for Listener {
        fn as_socket(&self) -> BorrowedSocket<'_> {
            match *self {
                Listener::Tcp(ref lst) => lst.as_socket(),
            }
        }
    }

    impl AsRawSocket for Listener {
        fn as_raw_socket(&self) -> RawSocket {
            match *self {
                Listener::Tcp(ref lst) => lst.as_raw_socket(),
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
        assert!(format!("{:?}", addr).contains("127.0.0.1:8080"));
        assert_eq!(format!("{}", addr), "127.0.0.1:8080");

        let addr: net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket.set_reuse_address(true).unwrap();
        socket.bind(&SockAddr::from(addr)).unwrap();
        let lst = Listener::Tcp(net::TcpListener::from(socket));
        assert!(format!("{:?}", lst).contains("TcpListener"));
        assert!(format!("{}", lst).contains("127.0.0.1"));
    }

    #[test]
    #[cfg(unix)]
    fn uds() {
        use std::os::unix::net::UnixListener;

        let _ = std::fs::remove_file("/tmp/sock.xxxxx");
        if let Ok(lst) = UnixListener::bind("/tmp/sock.xxxxx") {
            let addr = lst.local_addr().expect("Couldn't get local address");
            let a = SocketAddr::Uds(addr);
            assert!(format!("{:?}", a).contains("/tmp/sock.xxxxx"));
            assert!(format!("{}", a).contains("/tmp/sock.xxxxx"));

            let lst = Listener::Uds(lst);
            assert!(format!("{:?}", lst).contains("/tmp/sock.xxxxx"));
            assert!(format!("{}", lst).contains("/tmp/sock.xxxxx"));
        }
    }
}
