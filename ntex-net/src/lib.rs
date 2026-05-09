//! Utility for async runtime abstraction
#![deny(clippy::pedantic)]
#![allow(
    clippy::clone_on_copy,
    clippy::cast_possible_truncation,
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
use std::{io, net, net::SocketAddr, panic, panic::RefUnwindSafe, panic::UnwindSafe};

use ntex_io::Io;
use ntex_rt::{BlockFuture, Driver, Runner, remove_all_items};
use ntex_service::cfg::SharedCfg;

pub mod channel;
pub mod connect;

#[cfg(unix)]
pub mod polling;

#[cfg(target_os = "linux")]
pub mod uring;

#[cfg(unix)]
mod helpers;

#[cfg(feature = "tokio")]
pub mod tokio;

#[cfg(feature = "compio")]
mod compio;

#[allow(clippy::wrong_self_convention)]
pub trait Reactor: Driver {
    fn tcp_connect(&self, addr: net::SocketAddr, cfg: SharedCfg) -> channel::Receiver<Io>;

    fn unix_connect(
        &self,
        addr: std::path::PathBuf,
        cfg: SharedCfg,
    ) -> channel::Receiver<Io>;

    /// Convert std `TcpStream` to `Io`
    fn from_tcp_stream(&self, stream: net::TcpStream, cfg: SharedCfg) -> io::Result<Io>;

    #[cfg(unix)]
    /// Convert std `UnixStream` to `Io`
    fn from_unix_stream(
        &self,
        _: std::os::unix::net::UnixStream,
        _: SharedCfg,
    ) -> io::Result<Io>;
}

#[inline]
/// Opens a TCP connection to a remote host.
pub async fn tcp_connect(addr: SocketAddr, cfg: SharedCfg) -> io::Result<Io> {
    with_current(|driver| driver.tcp_connect(addr, cfg)).await
}

#[inline]
/// Opens a unix stream connection.
pub async fn unix_connect<'a, P>(addr: P, cfg: SharedCfg) -> io::Result<Io>
where
    P: AsRef<std::path::Path> + 'a,
{
    with_current(|driver| driver.unix_connect(addr.as_ref().into(), cfg)).await
}

#[inline]
/// Convert std `TcpStream` to `TcpStream`
pub fn from_tcp_stream(stream: net::TcpStream, cfg: SharedCfg) -> io::Result<Io> {
    with_current(|driver| driver.from_tcp_stream(stream, cfg))
}

#[cfg(unix)]
#[inline]
/// Convert std `UnixStream` to `UnixStream`
pub fn from_unix_stream(
    stream: std::os::unix::net::UnixStream,
    cfg: SharedCfg,
) -> io::Result<Io> {
    with_current(|driver| driver.from_unix_stream(stream, cfg))
}

fn with_current<T, F: FnOnce(&dyn Reactor) -> T>(f: F) -> T {
    #[cold]
    fn not_in_ntex_driver() -> ! {
        panic!("not in a ntex driver")
    }

    if CURRENT_DRIVER.is_set() {
        CURRENT_DRIVER.with(|d| f(&**d))
    } else {
        not_in_ntex_driver()
    }
}

scoped_tls::scoped_thread_local!(static CURRENT_DRIVER: Box<dyn Reactor + RefUnwindSafe>);

#[derive(Debug)]
pub struct DefaultRuntime;

impl Runner for DefaultRuntime {
    #[allow(unused_variables)]
    fn block_on(&self, fut: BlockFuture) {
        #[cfg(feature = "tokio")]
        {
            let driver: Box<dyn Reactor + RefUnwindSafe> =
                Box::new(self::tokio::TokioDriver);

            CURRENT_DRIVER.set(&driver, || {
                crate::tokio::block_on(fut);
                remove_all_items();
            });
        }

        #[cfg(all(feature = "compio", not(feature = "tokio")))]
        {
            let driver: Box<dyn Reactor + RefUnwindSafe> =
                Box::new(self::compio::CompioDriver);

            CURRENT_DRIVER.set(&driver, || {
                crate::compio::block_on(fut);
                remove_all_items();
            });
        }

        #[cfg(all(unix, not(feature = "tokio"), not(feature = "compio")))]
        {
            #[cfg(feature = "neon-polling")]
            {
                let driver =
                    crate::polling::Driver::new().expect("Cannot construct driver");
                let driver: Box<dyn Reactor + RefUnwindSafe> = Box::new(driver);

                let fut = BlockFutureWrapper(fut);

                CURRENT_DRIVER.set(&driver, || {
                    let rt = Wrapper(ntex_rt::Runtime::new(driver.handle()));
                    let res = std::panic::catch_unwind(|| rt.block_on(fut, &*driver));
                    if let Err(err) = res {
                        remove_all_items();
                        panic::resume_unwind(err);
                    } else {
                        driver.clear();
                        remove_all_items();
                    }
                });
            }

            #[cfg(all(target_os = "linux", feature = "neon-uring"))]
            {
                let driver =
                    crate::uring::Driver::new(2048).expect("Cannot construct driver");
                let driver: Box<dyn Reactor + RefUnwindSafe> = Box::new(driver);

                let fut = BlockFutureWrapper(fut);

                CURRENT_DRIVER.set(&driver, || {
                    let rt = Wrapper(ntex_rt::Runtime::new(driver.handle()));
                    let res = std::panic::catch_unwind(|| rt.block_on(fut, &*driver));
                    if let Err(err) = res {
                        remove_all_items();
                        panic::resume_unwind(err);
                    } else {
                        driver.clear();
                        remove_all_items();
                    }
                });
            }

            #[cfg(all(not(feature = "neon-uring"), not(feature = "neon-polling")))]
            {
                #[cfg(target_os = "linux")]
                let driver: Box<dyn Reactor + RefUnwindSafe> =
                    if let Ok(driver) = crate::uring::Driver::new(2048) {
                        Box::new(driver)
                    } else {
                        Box::new(
                            crate::polling::Driver::new().expect("Cannot construct driver"),
                        )
                    };

                #[cfg(not(target_os = "linux"))]
                let driver: Box<dyn Reactor + RefUnwindSafe> = Box::new(
                    crate::polling::Driver::new().expect("Cannot construct driver"),
                );

                let fut = BlockFutureWrapper(fut);

                CURRENT_DRIVER.set(&driver, || {
                    let rt = Wrapper(ntex_rt::Runtime::new(driver.handle()));
                    let res = std::panic::catch_unwind(|| rt.block_on(fut, &*driver));
                    if let Err(err) = res {
                        remove_all_items();
                        panic::resume_unwind(err);
                    } else {
                        driver.clear();
                        remove_all_items();
                    }
                });
            }
        }
    }
}

#[allow(dead_code)]
struct Wrapper(ntex_rt::Runtime);
#[allow(dead_code)]
struct BlockFutureWrapper(BlockFuture);

impl UnwindSafe for Wrapper {}
impl RefUnwindSafe for Wrapper {}
impl UnwindSafe for BlockFutureWrapper {}
impl RefUnwindSafe for BlockFutureWrapper {}

#[cfg(target_os = "linux")]
impl UnwindSafe for crate::uring::Driver {}
#[cfg(target_os = "linux")]
impl RefUnwindSafe for crate::uring::Driver {}

#[cfg(unix)]
impl UnwindSafe for crate::polling::Driver {}
#[cfg(unix)]
impl RefUnwindSafe for crate::polling::Driver {}

#[allow(dead_code)]
impl Wrapper {
    fn block_on(&self, fut: BlockFutureWrapper, driver: &dyn Driver) {
        self.0.block_on(fut.0, driver)
    }
}
