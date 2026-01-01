#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

use crate::channel::Receiver;

pub use tok_io::*;

pub(crate) struct TokioDriver;

impl ntex_rt::Driver for TokioDriver {
    fn run(&self, _: &ntex_rt::Runtime) -> std::io::Result<()> {
        panic!("Not supported")
    }

    fn handle(&self) -> Box<dyn ntex_rt::Notify> {
        panic!("Not supported")
    }

    fn clear(&self) {}
}

impl crate::Reactor for TokioDriver {
    fn tcp_connect(&self, addr: std::net::SocketAddr, cfg: SharedCfg) -> Receiver<Io> {
        let (tx, rx) = crate::channel::create();
        ntex_rt::spawn(async move {
            let result = async {
                let sock = tok_io::net::TcpStream::connect(addr).await?;
                sock.set_nodelay(true)?;
                Ok(Io::new(ntex_tokio::TcpStream::from(sock), cfg))
            }
            .await;
            let _ = tx.send(result);
        });

        rx
    }

    fn unix_connect(&self, _addr: std::path::PathBuf, _cfg: SharedCfg) -> Receiver<Io> {
        #[cfg(unix)]
        {
            let (tx, rx) = crate::channel::create();
            ntex_rt::spawn(async move {
                let result = async {
                    let sock = tok_io::net::UnixStream::connect(_addr).await?;
                    Ok(Io::new(ntex_tokio::UnixStream::from(sock), _cfg))
                }
                .await;
                let _ = tx.send(result);
            });

            rx
        }

        #[cfg(not(unix))]
        {
            Receiver::new(Err(std::io::Error::other(
                "Unix domain sockets are not supported",
            )))
        }
    }

    fn from_tcp_stream(
        &self,
        stream: std::net::TcpStream,
        cfg: SharedCfg,
    ) -> std::io::Result<Io> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        Ok(Io::new(
            ntex_tokio::TcpStream::from(tok_io::net::TcpStream::from_std(stream)?),
            cfg,
        ))
    }

    #[cfg(unix)]
    fn from_unix_stream(
        &self,
        stream: OsUnixStream,
        cfg: SharedCfg,
    ) -> std::io::Result<Io> {
        stream.set_nonblocking(true)?;
        Ok(Io::new(
            ntex_tokio::UnixStream::from(tok_io::net::UnixStream::from_std(stream)?),
            cfg,
        ))
    }
}
