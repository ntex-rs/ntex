use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fmt, thread};

use futures::sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::sync::oneshot::{channel, Canceled, Sender};
use futures::{future, Async, Future, IntoFuture, Poll, Stream};
use tokio_current_thread::spawn;

use crate::builder::Builder;
use crate::system::System;

use copyless::BoxHelper;

thread_local!(
    static ADDR: RefCell<Option<Arbiter>> = RefCell::new(None);
    static RUNNING: Cell<bool> = Cell::new(false);
    static Q: RefCell<Vec<Box<dyn Future<Item = (), Error = ()>>>> = RefCell::new(Vec::new());
    static STORAGE: RefCell<HashMap<TypeId, Box<dyn Any>>> = RefCell::new(HashMap::new());
);

pub(crate) static COUNT: AtomicUsize = AtomicUsize::new(0);

pub(crate) enum ArbiterCommand {
    Stop,
    Execute(Box<dyn Future<Item = (), Error = ()> + Send>),
    ExecuteFn(Box<dyn FnExec>),
}

impl fmt::Debug for ArbiterCommand {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ArbiterCommand::Stop => write!(f, "ArbiterCommand::Stop"),
            ArbiterCommand::Execute(_) => write!(f, "ArbiterCommand::Execute"),
            ArbiterCommand::ExecuteFn(_) => write!(f, "ArbiterCommand::ExecuteFn"),
        }
    }
}

#[derive(Debug, Clone)]
/// Arbiters provide an asynchronous execution environment for actors, functions
/// and futures. When an Arbiter is created, they spawn a new OS thread, and
/// host an event loop. Some Arbiter functions execute on the current thread.
pub struct Arbiter(UnboundedSender<ArbiterCommand>);

impl Default for Arbiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Arbiter {
    pub(crate) fn new_system() -> Self {
        let (tx, rx) = unbounded();

        let arb = Arbiter(tx);
        ADDR.with(|cell| *cell.borrow_mut() = Some(arb.clone()));
        RUNNING.with(|cell| cell.set(false));
        STORAGE.with(|cell| cell.borrow_mut().clear());
        Arbiter::spawn(ArbiterController { stop: None, rx });

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
        let _ = self.0.unbounded_send(ArbiterCommand::Stop);
    }

    /// Spawn new thread and run event loop in spawned thread.
    /// Returns address of newly created arbiter.
    pub fn new() -> Arbiter {
        let id = COUNT.fetch_add(1, Ordering::Relaxed);
        let name = format!("actix-rt:worker:{}", id);
        let sys = System::current();
        let (arb_tx, arb_rx) = unbounded();
        let arb_tx2 = arb_tx.clone();

        let _ = thread::Builder::new().name(name.clone()).spawn(move || {
            let mut rt = Builder::new().build_rt().expect("Can not create Runtime");
            let arb = Arbiter(arb_tx);

            let (stop, stop_rx) = channel();
            RUNNING.with(|cell| cell.set(true));
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
                .unbounded_send(SystemCommand::RegisterArbiter(id, arb.clone()));

            // run loop
            let _ = match rt.block_on(stop_rx) {
                Ok(code) => code,
                Err(_) => 1,
            };

            // unregister arbiter
            let _ = System::current()
                .sys()
                .unbounded_send(SystemCommand::UnregisterArbiter(id));
        });

        Arbiter(arb_tx2)
    }

    pub(crate) fn run_system() {
        RUNNING.with(|cell| cell.set(true));
        Q.with(|cell| {
            let mut v = cell.borrow_mut();
            for fut in v.drain(..) {
                spawn(fut);
            }
        });
    }

    pub(crate) fn stop_system() {
        RUNNING.with(|cell| cell.set(false));
    }

    /// Spawn a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for spawning futures on the current
    /// thread.
    pub fn spawn<F>(future: F)
    where
        F: Future<Item = (), Error = ()> + 'static,
    {
        RUNNING.with(move |cell| {
            if cell.get() {
                spawn(Box::alloc().init(future));
            } else {
                Q.with(move |cell| cell.borrow_mut().push(Box::alloc().init(future)));
            }
        });
    }

    /// Executes a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for executing futures on the current
    /// thread.
    pub fn spawn_fn<F, R>(f: F)
    where
        F: FnOnce() -> R + 'static,
        R: IntoFuture<Item = (), Error = ()> + 'static,
    {
        Arbiter::spawn(future::lazy(f))
    }

    /// Send a future to the Arbiter's thread, and spawn it.
    pub fn send<F>(&self, future: F)
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        let _ = self
            .0
            .unbounded_send(ArbiterCommand::Execute(Box::new(future)));
    }

    /// Send a function to the Arbiter's thread, and execute it. Any result from the function
    /// is discarded.
    pub fn exec_fn<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let _ = self
            .0
            .unbounded_send(ArbiterCommand::ExecuteFn(Box::new(move || {
                f();
            })));
    }

    /// Send a function to the Arbiter's thread. This function will be executed asynchronously.
    /// A future is created, and when resolved will contain the result of the function sent
    /// to the Arbiters thread.
    pub fn exec<F, R>(&self, f: F) -> impl Future<Item = R, Error = Canceled>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = channel();
        let _ = self
            .0
            .unbounded_send(ArbiterCommand::ExecuteFn(Box::new(move || {
                if !tx.is_canceled() {
                    let _ = tx.send(f());
                }
            })));
        rx
    }

    /// Set item to arbiter storage
    pub fn set_item<T: 'static>(item: T) {
        STORAGE.with(move |cell| cell.borrow_mut().insert(TypeId::of::<T>(), Box::new(item)));
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
                .and_then(|boxed| (&mut **boxed as &mut (dyn Any + 'static)).downcast_mut())
                .unwrap();
            f(item)
        })
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
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.rx.poll() {
                Ok(Async::Ready(None)) | Err(_) => return Ok(Async::Ready(())),
                Ok(Async::Ready(Some(item))) => match item {
                    ArbiterCommand::Stop => {
                        if let Some(stop) = self.stop.take() {
                            let _ = stop.send(0);
                        };
                        return Ok(Async::Ready(()));
                    }
                    ArbiterCommand::Execute(fut) => {
                        spawn(fut);
                    }
                    ArbiterCommand::ExecuteFn(f) => {
                        f.call_box();
                    }
                },
                Ok(Async::NotReady) => return Ok(Async::NotReady),
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum SystemCommand {
    Exit(i32),
    RegisterArbiter(usize, Arbiter),
    UnregisterArbiter(usize),
}

#[derive(Debug)]
pub(crate) struct SystemArbiter {
    stop: Option<Sender<i32>>,
    commands: UnboundedReceiver<SystemCommand>,
    arbiters: HashMap<usize, Arbiter>,
}

impl SystemArbiter {
    pub(crate) fn new(stop: Sender<i32>, commands: UnboundedReceiver<SystemCommand>) -> Self {
        SystemArbiter {
            commands,
            stop: Some(stop),
            arbiters: HashMap::new(),
        }
    }
}

impl Future for SystemArbiter {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.commands.poll() {
                Ok(Async::Ready(None)) | Err(_) => return Ok(Async::Ready(())),
                Ok(Async::Ready(Some(cmd))) => match cmd {
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
                Ok(Async::NotReady) => return Ok(Async::NotReady),
            }
        }
    }
}

pub trait FnExec: Send + 'static {
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
