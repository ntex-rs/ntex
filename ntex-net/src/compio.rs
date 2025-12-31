#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

use crate::channel::Receiver;

pub(crate) struct CompioDriver;

impl ntex_rt::Driver for CompioDriver {
    fn run(&self, _: &ntex_rt::Runtime) -> std::io::Result<()> {
        panic!("Not supported")
    }

    fn handle(&self) -> Box<dyn ntex_rt::Notify> {
        panic!("Not supported")
    }

    fn clear(&self) {}
}

impl crate::Reactor for CompioDriver {
    fn tcp_connect(&self, addr: std::net::SocketAddr, cfg: SharedCfg) -> Receiver<Io> {
        let (tx, rx) = crate::channel::create();
        ntex_rt::spawn(async move {
            let result = async {
                let sock = compio_net::TcpStream::connect(addr).await?;
                Ok(Io::new(ntex_compio::TcpStream::from(sock), cfg))
            }
            .await;
            let _ = tx.send(result);
        });

        rx
    }

    fn unix_connect(&self, addr: std::path::PathBuf, cfg: SharedCfg) -> Receiver<Io> {
        let (tx, rx) = crate::channel::create();
        ntex_rt::spawn(async move {
            let result = async {
                let sock = compio_net::UnixStream::connect(addr).await?;
                Ok(Io::new(ntex_compio::UnixStream::from(sock), cfg))
            }
            .await;
            let _ = tx.send(result);
        });

        rx
    }

    fn from_tcp_stream(
        &self,
        stream: std::net::TcpStream,
        cfg: SharedCfg,
    ) -> std::io::Result<Io> {
        stream.set_nodelay(true)?;
        Ok(Io::new(
            ntex_compio::TcpStream::from(compio_net::TcpStream::from_std(stream)?),
            cfg,
        ))
    }

    #[cfg(unix)]
    fn from_unix_stream(
        &self,
        stream: OsUnixStream,
        cfg: SharedCfg,
    ) -> std::io::Result<Io> {
        Ok(Io::new(
            ntex_compio::UnixStream::from(compio_net::UnixStream::from_std(stream)?),
            cfg,
        ))
    }
}
