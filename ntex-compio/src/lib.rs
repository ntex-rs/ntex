#[cfg(feature = "compio")]
mod io;

#[cfg(feature = "compio")]
pub(crate) mod compat {
    use std::{io::Result, net, net::SocketAddr};

    use ntex_io::Io;
    use ntex_service::cfg::SharedCfg;

    /// Tcp stream wrapper for compio TcpStream
    pub(crate) struct TcpStream(pub(crate) compio_net::TcpStream);

    /// Tcp stream wrapper for compio UnixStream
    pub(crate) struct UnixStream(pub(crate) compio_net::UnixStream);

    /// Opens a TCP connection to a remote host.
    pub async fn tcp_connect(addr: SocketAddr, cfg: SharedCfg) -> Result<Io> {
        let sock = compio_net::TcpStream::connect(addr).await?;
        Ok(Io::new(TcpStream(sock), cfg))
    }

    /// Opens a unix stream connection.
    pub async fn unix_connect<'a, P>(addr: P, cfg: SharedCfg) -> Result<Io>
    where
        P: AsRef<std::path::Path> + 'a,
    {
        let sock = compio_net::UnixStream::connect(addr).await?;
        Ok(Io::new(UnixStream(sock), cfg))
    }

    /// Convert std TcpStream to tokio's TcpStream
    pub fn from_tcp_stream(stream: net::TcpStream, cfg: SharedCfg) -> Result<Io> {
        stream.set_nodelay(true)?;
        Ok(Io::new(
            TcpStream(compio_net::TcpStream::from_std(stream)?),
            cfg,
        ))
    }

    #[cfg(unix)]
    /// Convert std UnixStream to tokio's UnixStream
    pub fn from_unix_stream(
        stream: std::os::unix::net::UnixStream,
        cfg: SharedCfg,
    ) -> Result<Io> {
        Ok(Io::new(
            UnixStream(compio_net::UnixStream::from_std(stream)?),
            cfg,
        ))
    }
}

#[cfg(feature = "compio")]
pub use self::compat::*;
