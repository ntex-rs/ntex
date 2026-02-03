#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

mod io_impl;

pub use self::io_impl::{SocketOptions, TokioIoBoxed};
use crate::channel::{self, Receiver};

#[doc(hidden)]
pub use tok_io::*;

pub(crate) struct TcpStream(tok_io::net::TcpStream);

#[cfg(unix)]
pub(crate) struct UnixStream(tok_io::net::UnixStream);

pub(crate) struct TokioDriver;

/// Runs the provided future, blocking the current thread until the future
/// completes.
pub(crate) fn block_on<F: Future<Output = ()>>(fut: F) {
    if let Ok(hnd) = tok_io::runtime::Handle::try_current() {
        log::debug!("Use existing tokio runtime and block on future");
        hnd.block_on(tok_io::task::LocalSet::new().run_until(fut));
    } else {
        log::debug!("Create tokio runtime and block on future");

        let rt = tok_io::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tok_io::task::LocalSet::new().block_on(&rt, fut);
    }
}

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
        let (tx, rx) = channel::create();
        ntex_rt::spawn(async move {
            let result = async {
                let sock = tok_io::net::TcpStream::connect(addr).await?;
                sock.set_nodelay(true)?;
                Ok(Io::new(TcpStream(sock), cfg))
            }
            .await;
            let _ = tx.send(result);
        });

        rx
    }

    fn unix_connect(&self, addr: std::path::PathBuf, cfg: SharedCfg) -> Receiver<Io> {
        #[cfg(unix)]
        {
            let (tx, rx) = channel::create();
            ntex_rt::spawn(async move {
                let result = async {
                    let sock = tok_io::net::UnixStream::connect(addr).await?;
                    Ok(Io::new(UnixStream(sock), cfg))
                }
                .await;
                let _ = tx.send(result);
            });

            rx
        }

        #[cfg(not(unix))]
        {
            drop(cfg);
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
            TcpStream(tok_io::net::TcpStream::from_std(stream)?),
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
            UnixStream(tok_io::net::UnixStream::from_std(stream)?),
            cfg,
        ))
    }
}
