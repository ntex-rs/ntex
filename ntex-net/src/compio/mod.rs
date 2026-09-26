#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use compio_runtime::Runtime;
use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

mod io;

use crate::channel::{self, Receiver};

/// Tcp stream wrapper for compio `TcpStream`
pub(crate) struct TcpStream(pub(crate) compio_net::TcpStream);

/// Tcp stream wrapper for compio `UnixStream`
pub(crate) struct UnixStream(pub(crate) compio_net::UnixStream);

/// Dropping the runtime drops its remaining tasks, a destructor must not spawn
/// a new task while the task queue is being cleared.
struct RtGuard(Runtime);

impl Drop for RtGuard {
    fn drop(&mut self) {
        ntex_rt::set_stopping(true);
    }
}

struct ResetGuard;

impl Drop for ResetGuard {
    fn drop(&mut self) {
        ntex_rt::set_stopping(false);
    }
}

/// Runs the provided future, blocking the current thread until the future
/// completes.
pub(crate) fn block_on<F: Future<Output = ()>>(fut: F) {
    log::info!(
        "Starting compio runtime, driver {:?}",
        compio_runtime::Runtime::try_with_current(Runtime::driver_type)
            .unwrap_or(compio_driver::DriverType::Poll)
    );
    let _reset = ResetGuard;
    let rt = RtGuard(Runtime::new().unwrap());
    rt.0.block_on(fut);
}

#[derive(Copy, Clone, Debug)]
/// ntex reactor based on compio runtime
pub struct Reactor;

impl ntex_rt::Driver for Reactor {
    fn run(&self, _: &ntex_rt::Runtime) -> std::io::Result<()> {
        panic!("Not supported")
    }

    fn handle(&self) -> Box<dyn ntex_rt::Notify> {
        panic!("Not supported")
    }

    fn clear(&self) {}
}

impl crate::Reactor for Reactor {
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

    fn from_tcp_stream(&self, stream: std::net::TcpStream, cfg: SharedCfg) -> std::io::Result<Io> {
        stream.set_nodelay(true)?;
        Ok(Io::new(
            TcpStream(compio_net::TcpStream::from_std(stream)?),
            cfg,
        ))
    }

    #[cfg(unix)]
    fn from_unix_stream(&self, stream: OsUnixStream, cfg: SharedCfg) -> std::io::Result<Io> {
        Ok(Io::new(
            UnixStream(compio_net::UnixStream::from_std(stream)?),
            cfg,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, future::pending, rc::Rc};

    use ntex_rt::System;

    use crate::DefaultRuntime;

    struct SpawnOnDrop;

    impl Drop for SpawnOnDrop {
        fn drop(&mut self) {
            ntex_rt::spawn(async {});
        }
    }

    /// The runtime drops the tasks still pending on shutdown, a task that spawns
    /// from its destructor must not insert into the task queue being cleared.
    #[test]
    fn spawn_from_task_destructor_on_shutdown() {
        std::thread::spawn(|| {
            System::new("test", DefaultRuntime).block_on(async {
                let guard = SpawnOnDrop;
                ntex_rt::spawn(async move {
                    let _guard = guard;
                    pending::<()>().await;
                });
            });

            // spawning works again for the next runtime on the thread
            let ran = Rc::new(Cell::new(false));
            let ran2 = ran.clone();
            System::new("test", DefaultRuntime).block_on(async move {
                ntex_rt::spawn(async move { ran2.set(true) }).await.unwrap();
            });
            assert!(ran.get());
        })
        .join()
        .unwrap();
    }
}
