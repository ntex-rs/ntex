#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use compio_runtime::Runtime;
use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

mod io_impl;

use crate::channel::{self, Receiver};

/// Tcp stream wrapper for compio TcpStream
pub(crate) struct TcpStream(pub(crate) compio_net::TcpStream);

/// Tcp stream wrapper for compio UnixStream
pub(crate) struct UnixStream(pub(crate) compio_net::UnixStream);

/// Runs the provided future, blocking the current thread until the future
/// completes.
pub(crate) fn block_on<F: Future<Output = ()>>(fut: F) {
    log::info!(
        "Starting compio runtime, driver {:?}",
        compio_runtime::Runtime::try_with_current(|rt| rt.driver_type())
            .unwrap_or(compio_driver::DriverType::Poll)
    );
    let rt = Runtime::new().unwrap();
    rt.block_on(fut);
}

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
        let (tx, rx) = channel::create();
        ntex_rt::spawn(async move {
            let result = async {
                let sock = compio_net::TcpStream::connect(addr).await?;
                Ok(Io::new(TcpStream(sock), cfg))
            }
            .await;
            let _ = tx.send(result);
        });

        rx
    }

    fn unix_connect(&self, addr: std::path::PathBuf, cfg: SharedCfg) -> Receiver<Io> {
        let (tx, rx) = channel::create();
        ntex_rt::spawn(async move {
            let result = async {
                let sock = compio_net::UnixStream::connect(addr).await?;
                Ok(Io::new(UnixStream(sock), cfg))
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
            TcpStream(compio_net::TcpStream::from_std(stream)?),
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
            UnixStream(compio_net::UnixStream::from_std(stream)?),
            cfg,
        ))
    }
}
