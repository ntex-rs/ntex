use std::{fmt, io, net};

use crate::codec::{AsyncRead, AsyncWrite};
use crate::rt::net::TcpStream;

pub(crate) enum Listener {
    Tcp(mio::net::TcpListener),
    #[cfg(unix)]
    Uds(mio::net::UnixListener),
}

pub(crate) enum SocketAddr {
    Tcp(net::SocketAddr),
    #[cfg(unix)]
    Uds(mio::net::SocketAddr),
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

impl Listener {
    pub(super) fn from_tcp(lst: net::TcpListener) -> Self {
        let _ = lst.set_nonblocking(true);
        Listener::Tcp(mio::net::TcpListener::from_std(lst))
    }

    #[cfg(unix)]
    pub(super) fn from_uds(lst: std::os::unix::net::UnixListener) -> Self {
        let _ = lst.set_nonblocking(true);
        Listener::Uds(mio::net::UnixListener::from_std(lst))
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
}

impl mio::event::Source for Listener {
    #[inline]
    fn register(
        &mut self,
        poll: &mio::Registry,
        token: mio::Token,
        interest: mio::Interest,
    ) -> io::Result<()> {
        match *self {
            Listener::Tcp(ref mut lst) => lst.register(poll, token, interest),
            #[cfg(unix)]
            Listener::Uds(ref mut lst) => lst.register(poll, token, interest),
        }
    }

    #[inline]
    fn reregister(
        &mut self,
        poll: &mio::Registry,
        token: mio::Token,
        interest: mio::Interest,
    ) -> io::Result<()> {
        match *self {
            Listener::Tcp(ref mut lst) => lst.reregister(poll, token, interest),
            #[cfg(unix)]
            Listener::Uds(ref mut lst) => lst.reregister(poll, token, interest),
        }
    }

    #[inline]
    fn deregister(&mut self, poll: &mio::Registry) -> io::Result<()> {
        match *self {
            Listener::Tcp(ref mut lst) => lst.deregister(poll),
            #[cfg(unix)]
            Listener::Uds(ref mut lst) => {
                let res = lst.deregister(poll);

                // cleanup file path
                if let Ok(addr) = lst.local_addr() {
                    if let Some(path) = addr.as_pathname() {
                        let _ = std::fs::remove_file(path);
                    }
                }
                res
            }
        }
    }
}

#[derive(Debug)]
pub enum Stream {
    Tcp(mio::net::TcpStream),
    #[cfg(unix)]
    Uds(mio::net::UnixStream),
}

pub trait FromStream: AsyncRead + AsyncWrite + Sized {
    fn from_stream(stream: Stream) -> io::Result<Self>;
}

#[cfg(unix)]
impl FromStream for TcpStream {
    fn from_stream(sock: Stream) -> io::Result<Self> {
        match sock {
            Stream::Tcp(stream) => {
                use std::os::unix::io::{FromRawFd, IntoRawFd};
                let fd = IntoRawFd::into_raw_fd(stream);
                let io = TcpStream::from_std(unsafe { FromRawFd::from_raw_fd(fd) })?;
                io.set_nodelay(true)?;
                Ok(io)
            }
            #[cfg(unix)]
            Stream::Uds(_) => {
                panic!("Should not happen, bug in server impl");
            }
        }
    }
}

#[cfg(windows)]
impl FromStream for TcpStream {
    fn from_stream(sock: Stream) -> io::Result<Self> {
        match sock {
            Stream::Tcp(stream) => {
                use std::os::windows::io::{FromRawSocket, IntoRawSocket};
                let fd = IntoRawSocket::into_raw_socket(stream);
                let io =
                    TcpStream::from_std(unsafe { FromRawSocket::from_raw_socket(fd) })?;
                io.set_nodelay(true)?;
                Ok(io)
            }
            #[cfg(unix)]
            Stream::Uds(_) => {
                panic!("Should not happen, bug in server impl");
            }
        }
    }
}

#[cfg(unix)]
impl FromStream for crate::rt::net::UnixStream {
    fn from_stream(sock: Stream) -> io::Result<Self> {
        match sock {
            Stream::Tcp(_) => panic!("Should not happen, bug in server impl"),
            Stream::Uds(stream) => {
                use crate::rt::net::UnixStream;
                use std::os::unix::io::{FromRawFd, IntoRawFd};
                let fd = IntoRawFd::into_raw_fd(stream);
                UnixStream::from_std(unsafe { FromRawFd::from_raw_fd(fd) })
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
        let tcp = net::TcpListener::from(socket);
        let lst = Listener::Tcp(mio::net::TcpListener::from_std(tcp));
        assert!(format!("{:?}", lst).contains("TcpListener"));
        assert!(format!("{}", lst).contains("127.0.0.1"));
    }

    #[test]
    #[cfg(all(unix))]
    fn uds() {
        use std::os::unix::net::UnixListener;

        let _ = std::fs::remove_file("/tmp/sock.xxxxx");
        if let Ok(lst) = UnixListener::bind("/tmp/sock.xxxxx") {
            let lst = mio::net::UnixListener::from_std(lst);
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
