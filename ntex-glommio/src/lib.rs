#[cfg(target_os = "linux")]
mod io;
#[cfg(target_os = "linux")]
mod signals;

#[cfg(target_os = "linux")]
pub use self::signals::{signal, Signal};

#[cfg(target_os = "linux")]
mod net_impl {
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::{cell::RefCell, io::Result, net, net::SocketAddr, rc::Rc};

    use ntex_bytes::PoolRef;
    use ntex_io::Io;

    pub type JoinError = futures_channel::oneshot::Canceled;

    #[derive(Clone)]
    pub(crate) struct TcpStream(pub(crate) Rc<RefCell<glommio::net::TcpStream>>);

    impl TcpStream {
        fn new(io: glommio::net::TcpStream) -> Self {
            Self(Rc::new(RefCell::new(io)))
        }
    }

    #[derive(Clone)]
    pub(crate) struct UnixStream(pub(crate) Rc<RefCell<glommio::net::UnixStream>>);

    impl UnixStream {
        fn new(io: glommio::net::UnixStream) -> Self {
            Self(Rc::new(RefCell::new(io)))
        }
    }

    /// Opens a TCP connection to a remote host.
    pub async fn tcp_connect(addr: SocketAddr) -> Result<Io> {
        let sock = glommio::net::TcpStream::connect(addr).await?;
        sock.set_nodelay(true)?;
        Ok(Io::new(TcpStream::new(sock)))
    }

    /// Opens a TCP connection to a remote host and use specified memory pool.
    pub async fn tcp_connect_in(addr: SocketAddr, pool: PoolRef) -> Result<Io> {
        let sock = glommio::net::TcpStream::connect(addr).await?;
        sock.set_nodelay(true)?;
        Ok(Io::with_memory_pool(TcpStream::new(sock), pool))
    }

    /// Opens a unix stream connection.
    pub async fn unix_connect<P>(addr: P) -> Result<Io>
    where
        P: AsRef<std::path::Path>,
    {
        let sock = glommio::net::UnixStream::connect(addr).await?;
        Ok(Io::new(UnixStream::new(sock)))
    }

    /// Opens a unix stream connection and specified memory pool.
    pub async fn unix_connect_in<P>(addr: P, pool: PoolRef) -> Result<Io>
    where
        P: AsRef<std::path::Path>,
    {
        let sock = glommio::net::UnixStream::connect(addr).await?;
        Ok(Io::with_memory_pool(UnixStream::new(sock), pool))
    }

    /// Convert std TcpStream to glommio's TcpStream
    pub fn from_tcp_stream(stream: net::TcpStream) -> Result<Io> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        unsafe {
            Ok(Io::new(TcpStream::new(
                glommio::net::TcpStream::from_raw_fd(stream.into_raw_fd()),
            )))
        }
    }

    /// Convert std UnixStream to glommio's UnixStream
    pub fn from_unix_stream(stream: std::os::unix::net::UnixStream) -> Result<Io> {
        stream.set_nonblocking(true)?;
        // Ok(Io::new(UnixStream::new(From::from(stream))))
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Cannot creat glommio UnixStream from std type",
        ))
    }
}

#[cfg(target_os = "linux")]
pub use self::net_impl::*;
