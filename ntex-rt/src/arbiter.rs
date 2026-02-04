#![allow(clippy::missing_panics_doc)]
use std::any::{Any, TypeId};
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::{cell::RefCell, collections::HashMap, fmt, future::Future, pin::Pin, thread};

use async_channel::{Receiver, Sender, unbounded};

use crate::system::{FnExec, Id, System, SystemCommand};

thread_local!(
    static ADDR: RefCell<Option<Arbiter>> = const { RefCell::new(None) };
    static STORAGE: RefCell<HashMap<TypeId, Box<dyn Any>>> = RefCell::new(HashMap::new());
);

pub(super) static COUNT: AtomicUsize = AtomicUsize::new(0);

pub(super) enum ArbiterCommand {
    Stop,
    Execute(Pin<Box<dyn Future<Output = ()> + Send>>),
    ExecuteFn(Box<dyn FnExec>),
}

/// Arbiters provide an asynchronous execution environment for actors, functions
/// and futures.
///
/// When an Arbiter is created, it spawns a new OS thread, and
/// hosts an event loop. Some Arbiter functions execute on the current thread.
pub struct Arbiter {
    id: usize,
    pub(crate) sys_id: usize,
    name: Arc<String>,
    sender: Sender<ArbiterCommand>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

impl fmt::Debug for Arbiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Arbiter({:?})", self.name.as_ref())
    }
}

impl Default for Arbiter {
    fn default() -> Arbiter {
        Arbiter::new()
    }
}

impl Clone for Arbiter {
    fn clone(&self) -> Self {
        Self::with_sender(self.sys_id, self.id, self.name.clone(), self.sender.clone())
    }
}

impl Arbiter {
    #[allow(clippy::borrowed_box)]
    pub(super) fn new_system(name: String) -> (Self, ArbiterController) {
        let (tx, rx) = unbounded();

        let arb = Arbiter::with_sender(0, 0, Arc::new(name), tx);
        ADDR.with(|cell| *cell.borrow_mut() = Some(arb.clone()));
        STORAGE.with(|cell| cell.borrow_mut().clear());

        (arb, ArbiterController { rx, stop: None })
    }

    /// Returns the current thread's arbiter's address
    ///
    /// # Panics
    ///
    /// Panics if Arbiter is not running
    pub fn current() -> Arbiter {
        ADDR.with(|cell| match *cell.borrow() {
            Some(ref addr) => addr.clone(),
            None => panic!("Arbiter is not running"),
        })
    }

    /// Stop arbiter from continuing it's event loop.
    pub fn stop(&self) {
        let _ = self.sender.try_send(ArbiterCommand::Stop);
    }

    /// Spawn new thread and run runtime in spawned thread.
    /// Returns address of newly created arbiter.
    pub fn new() -> Arbiter {
        let id = COUNT.load(Ordering::Relaxed) + 1;
        Arbiter::with_name(format!("{}:arb:{}", System::current().name(), id))
    }

    /// Spawn new thread and run runtime in spawned thread
    ///
    /// Returns address of newly created arbiter.
    pub fn with_name(name: String) -> Arbiter {
        let id = COUNT.fetch_add(1, Ordering::Relaxed);
        let sys = System::current();
        let name2 = Arc::new(name.clone());
        let config = sys.config();
        let (arb_tx, arb_rx) = unbounded();
        let arb_tx2 = arb_tx.clone();

        let builder = if sys.config().stack_size > 0 {
            thread::Builder::new()
                .name(name)
                .stack_size(sys.config().stack_size)
        } else {
            thread::Builder::new().name(name)
        };

        let name = name2.clone();
        let sys_id = sys.id();

        let handle = builder
            .spawn(move || {
                log::info!("Starting {name2:?} arbiter");

                let arb = Arbiter::with_sender(sys_id.0, id, name2, arb_tx);

                let (stop, stop_rx) = oneshot::channel();
                STORAGE.with(|cell| cell.borrow_mut().clear());

                System::set_current(sys);

                config.block_on(async move {
                    // start arbiter controller
                    crate::spawn(
                        ArbiterController {
                            stop: Some(stop),
                            rx: arb_rx,
                        }
                        .run(),
                    );
                    ADDR.with(|cell| *cell.borrow_mut() = Some(arb.clone()));

                    // register arbiter
                    let _ = System::current()
                        .sys()
                        .try_send(SystemCommand::RegisterArbiter(Id(id), arb));

                    // run loop
                    let _ = stop_rx.await;
                });

                // unregister arbiter
                let _ = System::current()
                    .sys()
                    .try_send(SystemCommand::UnregisterArbiter(Id(id)));
            })
            .unwrap_or_else(|err| {
                panic!("Cannot spawn an arbiter's thread {:?}: {:?}", &name, err)
            });

        Arbiter {
            id,
            name,
            sys_id: sys_id.0,
            sender: arb_tx2,
            thread_handle: Some(handle),
        }
    }

    fn with_sender(
        sys_id: usize,
        id: usize,
        name: Arc<String>,
        sender: Sender<ArbiterCommand>,
    ) -> Self {
        Self {
            id,
            sys_id,
            name,
            sender,
            thread_handle: None,
        }
    }

    /// Id of the arbiter
    pub fn id(&self) -> Id {
        Id(self.id)
    }

    /// Name of the arbiter
    pub fn name(&self) -> &str {
        self.name.as_ref()
    }

    /// Send a future to the Arbiter's thread, and spawn it.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let _ = self
            .sender
            .try_send(ArbiterCommand::Execute(Box::pin(future)));
    }

    #[rustfmt::skip]
    /// Send a function to the Arbiter's thread and spawns it's resulting future.
    /// This can be used to spawn non-send futures on the arbiter thread.
    pub fn spawn_with<F, R, O>(
        &self,
        f: F
    ) -> impl Future<Output = Result<O, oneshot::RecvError>> + Send + 'static
    where
        F: FnOnce() -> R + Send + 'static,
        R: Future<Output = O> + 'static,
        O: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender
            .try_send(ArbiterCommand::ExecuteFn(Box::new(move || {
                crate::spawn(async move {
                    let _ = tx.send(f().await);
                });
            })));
        rx
    }

    #[rustfmt::skip]
    /// Send a function to the Arbiter's thread. This function will be executed asynchronously.
    /// A future is created, and when resolved will contain the result of the function sent
    /// to the Arbiters thread.
    pub fn exec<F, R>(&self, f: F) -> impl Future<Output = Result<R, oneshot::RecvError>> + Send + 'static
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender
            .try_send(ArbiterCommand::ExecuteFn(Box::new(move || {
                let _ = tx.send(f());
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
            .try_send(ArbiterCommand::ExecuteFn(Box::new(move || {
                f();
            })));
    }

    /// Set item to current arbiter's storage
    pub fn set_item<T: 'static>(item: T) {
        STORAGE
            .with(move |cell| cell.borrow_mut().insert(TypeId::of::<T>(), Box::new(item)));
    }

    /// Check if arbiter storage contains item
    pub fn contains_item<T: 'static>() -> bool {
        STORAGE.with(move |cell| cell.borrow().get(&TypeId::of::<T>()).is_some())
    }

    /// Get a reference to a type previously inserted on this arbiter's storage
    ///
    /// # Panics
    ///
    /// Panics if item is not inserted
    pub fn get_item<T: 'static, F, R>(f: F) -> R
    where
        F: FnOnce(&T) -> R,
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

    /// Get a type previously inserted to this runtime or create new one.
    pub fn get_value<T, F>(f: F) -> T
    where
        T: Clone + 'static,
        F: FnOnce() -> T,
    {
        STORAGE.with(move |cell| {
            let mut st = cell.borrow_mut();
            if let Some(boxed) = st.get(&TypeId::of::<T>())
                && let Some(val) = (&**boxed as &(dyn Any + 'static)).downcast_ref::<T>()
            {
                return val.clone();
            }
            let val = f();
            st.insert(TypeId::of::<T>(), Box::new(val.clone()));
            val
        })
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

impl Eq for Arbiter {}

impl PartialEq for Arbiter {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.sys_id == other.sys_id
    }
}

pub(crate) struct ArbiterController {
    stop: Option<oneshot::Sender<i32>>,
    rx: Receiver<ArbiterCommand>,
}

impl Drop for ArbiterController {
    fn drop(&mut self) {
        if thread::panicking() {
            if System::current().stop_on_panic() {
                eprintln!("Panic in Arbiter thread, shutting down system.");
                System::current().stop_with_code(1);
            } else {
                eprintln!("Panic in Arbiter thread.");
            }
        }
    }
}

impl ArbiterController {
    pub(super) async fn run(mut self) {
        loop {
            match self.rx.recv().await {
                Ok(ArbiterCommand::Stop) => {
                    if let Some(stop) = self.stop.take() {
                        let _ = stop.send(0);
                    }
                    break;
                }
                Ok(ArbiterCommand::Execute(fut)) => {
                    crate::spawn(fut);
                }
                Ok(ArbiterCommand::ExecuteFn(f)) => {
                    f.call_box();
                }
                Err(_) => break,
            }
        }
    }
}
