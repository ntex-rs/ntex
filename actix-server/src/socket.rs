use std::{fmt, io, net};

use actix_codec::{AsyncRead, AsyncWrite};
use actix_rt::net::TcpStream;

pub(crate) enum StdListener {
    Tcp(net::TcpListener),
    #[cfg(all(unix))]
    Uds(std::os::unix::net::UnixListener),
}

pub(crate) enum SocketAddr {
    Tcp(net::SocketAddr),
    #[cfg(all(unix))]
    Uds(std::os::unix::net::SocketAddr),
}

impl fmt::Display for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SocketAddr::Tcp(ref addr) => write!(f, "{}", addr),
            #[cfg(all(unix))]
            SocketAddr::Uds(ref addr) => write!(f, "{:?}", addr),
        }
    }
}

impl fmt::Debug for SocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SocketAddr::Tcp(ref addr) => write!(f, "{:?}", addr),
            #[cfg(all(unix))]
            SocketAddr::Uds(ref addr) => write!(f, "{:?}", addr),
        }
    }
}

impl fmt::Display for StdListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            StdListener::Tcp(ref lst) => write!(f, "{}", lst.local_addr().ok().unwrap()),
            #[cfg(all(unix))]
            StdListener::Uds(ref lst) => write!(f, "{:?}", lst.local_addr().ok().unwrap()),
        }
    }
}

impl StdListener {
    pub(crate) fn local_addr(&self) -> SocketAddr {
        match self {
            StdListener::Tcp(lst) => SocketAddr::Tcp(lst.local_addr().unwrap()),
            #[cfg(all(unix))]
            StdListener::Uds(lst) => SocketAddr::Uds(lst.local_addr().unwrap()),
        }
    }

    pub(crate) fn into_listener(self) -> SocketListener {
        match self {
            StdListener::Tcp(lst) => SocketListener::Tcp(
                mio::net::TcpListener::from_std(lst)
                    .expect("Can not create mio::net::TcpListener"),
            ),
            #[cfg(all(unix))]
            StdListener::Uds(lst) => SocketListener::Uds(
                mio_uds::UnixListener::from_listener(lst)
                    .expect("Can not create mio_uds::UnixListener"),
            ),
        }
    }
}

#[derive(Debug)]
pub enum StdStream {
    Tcp(std::net::TcpStream),
    #[cfg(all(unix))]
    Uds(std::os::unix::net::UnixStream),
}

pub(crate) enum SocketListener {
    Tcp(mio::net::TcpListener),
    #[cfg(all(unix))]
    Uds(mio_uds::UnixListener),
}

impl SocketListener {
    pub(crate) fn accept(&self) -> io::Result<Option<(StdStream, SocketAddr)>> {
        match *self {
            SocketListener::Tcp(ref lst) => lst
                .accept_std()
                .map(|(stream, addr)| Some((StdStream::Tcp(stream), SocketAddr::Tcp(addr)))),
            #[cfg(all(unix))]
            SocketListener::Uds(ref lst) => lst.accept_std().map(|res| {
                res.map(|(stream, addr)| (StdStream::Uds(stream), SocketAddr::Uds(addr)))
            }),
        }
    }
}

impl mio::Evented for SocketListener {
    fn register(
        &self,
        poll: &mio::Poll,
        token: mio::Token,
        interest: mio::Ready,
        opts: mio::PollOpt,
    ) -> io::Result<()> {
        match *self {
            SocketListener::Tcp(ref lst) => lst.register(poll, token, interest, opts),
            #[cfg(all(unix))]
            SocketListener::Uds(ref lst) => lst.register(poll, token, interest, opts),
        }
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: mio::Token,
        interest: mio::Ready,
        opts: mio::PollOpt,
    ) -> io::Result<()> {
        match *self {
            SocketListener::Tcp(ref lst) => lst.reregister(poll, token, interest, opts),
            #[cfg(all(unix))]
            SocketListener::Uds(ref lst) => lst.reregister(poll, token, interest, opts),
        }
    }
    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        match *self {
            SocketListener::Tcp(ref lst) => lst.deregister(poll),
            #[cfg(all(unix))]
            SocketListener::Uds(ref lst) => {
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

pub trait FromStream: AsyncRead + AsyncWrite + Sized {
    fn from_stdstream(sock: StdStream) -> io::Result<Self>;
}

impl FromStream for TcpStream {
    fn from_stdstream(sock: StdStream) -> io::Result<Self> {
        match sock {
            StdStream::Tcp(stream) => TcpStream::from_std(stream),
            #[cfg(all(unix))]
            StdStream::Uds(_) => {
                panic!("Should not happen, bug in server impl");
            }
        }
    }
}

#[cfg(all(unix))]
impl FromStream for actix_rt::net::UnixStream {
    fn from_stdstream(sock: StdStream) -> io::Result<Self> {
        match sock {
            StdStream::Tcp(_) => panic!("Should not happen, bug in server impl"),
            StdStream::Uds(stream) => actix_rt::net::UnixStream::from_std(stream),
        }
    }
}
