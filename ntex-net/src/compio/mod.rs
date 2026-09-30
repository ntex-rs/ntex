#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;
use std::pin::Pin;
use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};
use std::task::{Context, Poll, Wake, Waker};

use compio_runtime::Runtime;
use ntex_io::Io;
use ntex_service::cfg::SharedCfg;

mod io;

use crate::channel::{self, Receiver};

// compio's IOCP driver reports socket failures as Win32 network errors,
// which `io::Error::kind()` does not classify.
#[cfg(windows)]
use crate::iocp::map_socket_error;

#[cfg(not(windows))]
fn map_socket_error(err: std::io::Error) -> std::io::Error {
    err
}

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
    let _reset = ResetGuard;
    let rt = RtGuard(Runtime::new().unwrap());
    log::info!("Starting compio runtime, driver {:?}", rt.0.driver_type());
    rt.0.block_on(PollWhenWoken {
        fut: Box::pin(fut),
        flag: None,
    });
}

/// Polls the future only after it has been woken.
///
/// compio's `block_on` polls the future on every turn of its loop, not only
/// when it was woken. A future that wakes another task whenever it is
/// pending would then wake a task on every turn. Every task wakeup also wakes
/// the driver, and a woken IOCP driver skips polling for I/O completions, so
/// I/O would never complete.
struct PollWhenWoken<F> {
    fut: Pin<Box<F>>,
    flag: Option<(Arc<WakeFlag>, Waker)>,
}

struct WakeFlag {
    woken: AtomicBool,
    waker: Waker,
}

impl Wake for WakeFlag {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        self.waker.wake_by_ref();
    }
}

impl<F: Future> Future for PollWhenWoken<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();

        if let Some((flag, _)) = &this.flag
            && flag.waker.will_wake(cx.waker())
        {
            // cleared before polling, a wakeup during the poll is kept
            if !flag.woken.swap(false, Ordering::Acquire) {
                return Poll::Pending;
            }
        } else {
            let flag = Arc::new(WakeFlag {
                woken: AtomicBool::new(false),
                waker: cx.waker().clone(),
            });
            let waker = Waker::from(flag.clone());
            this.flag = Some((flag, waker));
        }

        let waker = &this.flag.as_ref().unwrap().1;
        this.fut.as_mut().poll(&mut Context::from_waker(waker))
    }
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
                let sock = compio_net::TcpStream::connect(addr)
                    .await
                    .map_err(map_socket_error)?;
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
                let sock = compio_net::UnixStream::connect(addr)
                    .await
                    .map_err(map_socket_error)?;
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

    /// compio polls the `block_on` future on every turn of its loop. A future
    /// that wakes another task whenever it is polled, as the h2 payload does
    /// for its connection, must not keep the driver from completing I/O.
    #[test]
    fn io_completes_while_main_future_wakes_tasks() {
        use std::{cell::RefCell, future::poll_fn, io::Write, pin::pin, task::Poll, task::Waker};

        use ntex::codec::BytesCodec;
        use ntex_service::cfg::SharedCfg;
        use ntex_util::time::{Millis, timeout};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // the read is issued before the data arrives
            std::thread::sleep(std::time::Duration::from_millis(100));
            sock.write_all(b"hello").unwrap();
            sock
        });

        System::new("test", DefaultRuntime).block_on(async move {
            let task: Rc<RefCell<Option<Waker>>> = Rc::default();
            let task2 = task.clone();
            ntex_rt::spawn(poll_fn(move |cx| {
                *task2.borrow_mut() = Some(cx.waker().clone());
                Poll::<()>::Pending
            }));

            let io = crate::tcp_connect(addr, SharedCfg::default())
                .await
                .unwrap();
            let mut recv = pin!(io.recv(&BytesCodec));
            let msg = timeout(
                Millis(5_000),
                poll_fn(|cx| {
                    if let Some(waker) = task.borrow().as_ref() {
                        waker.wake_by_ref();
                    }
                    recv.as_mut().poll(cx)
                }),
            )
            .await
            .expect("read did not complete");
            assert_eq!(msg.unwrap().unwrap(), "hello");
        });
        drop(srv.join().unwrap());
    }
}
