use std::any::{Any, TypeId};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::{cell::RefCell, collections::HashMap, fmt, future::Future, pin::Pin, thread};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot::{channel, error::RecvError, Sender};
use tokio::task::LocalSet;

use super::runtime::Runtime;
use super::system::System;

thread_local!(
    static ADDR: RefCell<Option<Arbiter>> = RefCell::new(None);
    static STORAGE: RefCell<HashMap<TypeId, Box<dyn Any>>> =
        RefCell::new(HashMap::new());
);

pub(super) static COUNT: AtomicUsize = AtomicUsize::new(0);

pub(super) enum ArbiterCommand {
    Stop,
    Execute(Box<dyn Future<Output = ()> + Unpin + Send>),
    ExecuteFn(Box<dyn FnExec>),
}

/// Arbiters provide an asynchronous execution environment for actors, functions
/// and futures. When an Arbiter is created, it spawns a new OS thread, and
/// hosts an event loop. Some Arbiter functions execute on the current thread.
pub struct Arbiter {
    sender: UnboundedSender<ArbiterCommand>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

impl fmt::Debug for Arbiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Arbiter")
    }
}

impl Clone for Arbiter {
    fn clone(&self) -> Self {
        Self::with_sender(self.sender.clone())
    }
}

impl Default for Arbiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Arbiter {
    pub(super) fn new_system(local: &LocalSet) -> Self {
        let (tx, rx) = unbounded_channel();

        let arb = Arbiter::with_sender(tx);
        ADDR.with(|cell| *cell.borrow_mut() = Some(arb.clone()));
        STORAGE.with(|cell| cell.borrow_mut().clear());

        local.spawn_local(ArbiterController { stop: None, rx });

        arb
    }

    /// Returns the current thread's arbiter's address. If no Arbiter is present, then this
    /// function will panic!
    pub fn current() -> Arbiter {
        ADDR.with(|cell| match *cell.borrow() {
            Some(ref addr) => addr.clone(),
            None => panic!("Arbiter is not running"),
        })
    }

    /// Stop arbiter from continuing it's event loop.
    pub fn stop(&self) {
        let _ = self.sender.send(ArbiterCommand::Stop);
    }

    /// Spawn new thread and run event loop in spawned thread.
    /// Returns address of newly created arbiter.
    pub fn new() -> Arbiter {
        let id = COUNT.fetch_add(1, Ordering::Relaxed);
        let name = format!("ntex-rt:worker:{}", id);
        let sys = System::current();
        let (arb_tx, arb_rx) = unbounded_channel();
        let arb_tx2 = arb_tx.clone();

        let handle = thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                let rt = Runtime::new().expect("Cannot create Runtime");
                let arb = Arbiter::with_sender(arb_tx);

                let (stop, stop_rx) = channel();
                STORAGE.with(|cell| cell.borrow_mut().clear());

                System::set_current(sys);

                // start arbiter controller
                rt.spawn(ArbiterController {
                    stop: Some(stop),
                    rx: arb_rx,
                });
                ADDR.with(|cell| *cell.borrow_mut() = Some(arb.clone()));

                // register arbiter
                let _ = System::current()
                    .sys()
                    .send(SystemCommand::RegisterArbiter(id, arb));

                // run loop
                let _ = rt.block_on(stop_rx);

                // unregister arbiter
                let _ = System::current()
                    .sys()
                    .send(SystemCommand::UnregisterArbiter(id));
            })
            .unwrap_or_else(|err| {
                panic!("Cannot spawn an arbiter's thread {:?}: {:?}", &name, err)
            });

        Arbiter {
            sender: arb_tx2,
            thread_handle: Some(handle),
        }
    }

    /// Send a future to the Arbiter's thread, and spawn it.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + Unpin + 'static,
    {
        let _ = self.sender.send(ArbiterCommand::Execute(Box::new(future)));
    }

    /// Send a function to the Arbiter's thread. This function will be executed asynchronously.
    /// A future is created, and when resolved will contain the result of the function sent
    /// to the Arbiters thread.
    pub fn exec<F, R>(&self, f: F) -> impl Future<Output = Result<R, RecvError>>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = channel();
        let _ = self
            .sender
            .send(ArbiterCommand::ExecuteFn(Box::new(move || {
                if !tx.is_closed() {
                    let _ = tx.send(f());
                }
            })));
        rx
    }

    /// Send a function to the Arbiter's thread, and execute it. Any result from the function
    /// is discarded.
    pub fn exec_fn<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let _ = self
            .sender
            .send(ArbiterCommand::ExecuteFn(Box::new(move || {
                f();
            })));
    }

    /// Set item to current arbiter's storage
    pub fn set_item<T: 'static>(item: T) {
        STORAGE.with(move |cell| {
            cell.borrow_mut().insert(TypeId::of::<T>(), Box::new(item))
        });
    }

    /// Check if arbiter storage contains item
    pub fn contains_item<T: 'static>() -> bool {
        STORAGE.with(move |cell| cell.borrow().get(&TypeId::of::<T>()).is_some())
    }

    /// Get a reference to a type previously inserted on this arbiter's storage.
    ///
    /// Panics is item is not inserted
    pub fn get_item<T: 'static, F, R>(mut f: F) -> R
    where
        F: FnMut(&T) -> R,
    {
        STORAGE.with(move |cell| {
            let st = cell.borrow();
            let item = st
                .get(&TypeId::of::<T>())
                .and_then(|boxed| (&**boxed as &(dyn Any + 'static)).downcast_ref())
                .unwrap();
            f(item)
        })
    }

    /// Get a mutable reference to a type previously inserted on this arbiter's storage.
    ///
    /// Panics is item is not inserted
    pub fn get_mut_item<T: 'static, F, R>(mut f: F) -> R
    where
        F: FnMut(&mut T) -> R,
    {
        STORAGE.with(move |cell| {
            let mut st = cell.borrow_mut();
            let item = st
                .get_mut(&TypeId::of::<T>())
                .and_then(|boxed| {
                    (&mut **boxed as &mut (dyn Any + 'static)).downcast_mut()
                })
                .unwrap();
            f(item)
        })
    }

    fn with_sender(sender: UnboundedSender<ArbiterCommand>) -> Self {
        Self {
            sender,
            thread_handle: None,
        }
    }

    /// Wait for the event loop to stop by joining the underlying thread (if have Some).
    pub fn join(&mut self) -> thread::Result<()> {
        if let Some(thread_handle) = self.thread_handle.take() {
            thread_handle.join()
        } else {
            Ok(())
        }
    }
}

struct ArbiterController {
    stop: Option<Sender<i32>>,
    rx: UnboundedReceiver<ArbiterCommand>,
}

impl Drop for ArbiterController {
    fn drop(&mut self) {
        if thread::panicking() {
            if System::current().stop_on_panic() {
                eprintln!("Panic in Arbiter thread, shutting down system.");
                System::current().stop_with_code(1)
            } else {
                eprintln!("Panic in Arbiter thread.");
            }
        }
    }
}

impl Future for ArbiterController {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match Pin::new(&mut self.rx).poll_recv(cx) {
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Ready(Some(item)) => match item {
                    ArbiterCommand::Stop => {
                        if let Some(stop) = self.stop.take() {
                            let _ = stop.send(0);
                        };
                        return Poll::Ready(());
                    }
                    ArbiterCommand::Execute(fut) => {
                        tokio::task::spawn_local(fut);
                    }
                    ArbiterCommand::ExecuteFn(f) => {
                        f.call_box();
                    }
                },
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Debug)]
pub(super) enum SystemCommand {
    Exit(i32),
    RegisterArbiter(usize, Arbiter),
    UnregisterArbiter(usize),
}

#[derive(Debug)]
pub(super) struct SystemArbiter {
    stop: Option<Sender<i32>>,
    commands: UnboundedReceiver<SystemCommand>,
    arbiters: HashMap<usize, Arbiter>,
}

impl SystemArbiter {
    pub(super) fn new(
        stop: Sender<i32>,
        commands: UnboundedReceiver<SystemCommand>,
    ) -> Self {
        SystemArbiter {
            commands,
            stop: Some(stop),
            arbiters: HashMap::new(),
        }
    }
}

impl Future for SystemArbiter {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match Pin::new(&mut self.commands).poll_recv(cx) {
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Ready(Some(cmd)) => match cmd {
                    SystemCommand::Exit(code) => {
                        // stop arbiters
                        for arb in self.arbiters.values() {
                            arb.stop();
                        }
                        // stop event loop
                        if let Some(stop) = self.stop.take() {
                            let _ = stop.send(code);
                        }
                    }
                    SystemCommand::RegisterArbiter(name, hnd) => {
                        self.arbiters.insert(name, hnd);
                    }
                    SystemCommand::UnregisterArbiter(name) => {
                        self.arbiters.remove(&name);
                    }
                },
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub(super) trait FnExec: Send + 'static {
    fn call_box(self: Box<Self>);
}

impl<F> FnExec for F
where
    F: FnOnce() + Send + 'static,
{
    #[allow(clippy::boxed_local)]
    fn call_box(self: Box<Self>) {
        (*self)()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arbiter_local_storage() {
        let _s = System::new("test");
        Arbiter::set_item("test");
        assert!(Arbiter::get_item::<&'static str, _, _>(|s| *s == "test"));
        assert!(Arbiter::get_mut_item::<&'static str, _, _>(|s| *s == "test"));
        assert!(format!("{:?}", Arbiter::current()).contains("Arbiter"));
    }
}
