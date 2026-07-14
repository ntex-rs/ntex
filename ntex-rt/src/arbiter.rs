#![allow(clippy::missing_panics_doc)]
use std::sync::{Arc, atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering};
use std::{any::Any, any::TypeId, cell::RefCell, fmt, pin::Pin, thread};

use async_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;

use crate::{Handle, HashMap, Id, System};

thread_local!(
    static ADDR: RefCell<Option<Arbiter>> = const { RefCell::new(None) };
    static STORAGE: RefCell<HashMap<TypeId, Box<dyn Any>>> =
        RefCell::new(HashMap::default());
);

pub(super) static COUNT: AtomicUsize = AtomicUsize::new(99);

pub(super) enum ArbiterCommand {
    Stop,
    #[allow(dead_code)]
    Execute(Pin<Box<dyn Future<Output = ()> + Send>>),
}

/// Arbiters provide an asynchronous execution environment for actors, functions
/// and futures.
///
/// When an Arbiter is created, it spawns a new OS thread, and
/// hosts an event loop. Some Arbiter functions execute on the current thread.
pub struct Arbiter(pub(crate) Arc<ArbiterInner>);

pub(crate) struct ArbiterInner {
    id: usize,
    name: Arc<String>,
    sys_id: usize,
    hnd: Option<Handle>,
    pub(crate) sender: Sender<ArbiterCommand>,
    thread_handle: Mutex<Option<thread::JoinHandle<()>>>,
    running: AtomicBool,
    #[cfg(target_os = "linux")]
    tid: i32,
}

impl fmt::Debug for Arbiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Arbiter({:?})", self.0.name.as_ref())
    }
}

impl Clone for Arbiter {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Default for Arbiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Arbiter {
    #[allow(clippy::borrowed_box)]
    pub(super) fn new_system(id: usize, name: String) -> (Self, ArbiterController) {
        let (tx, rx) = unbounded();

        let aid = COUNT.fetch_add(1, Ordering::Relaxed);
        let arb = Arbiter::with_sender(id, aid, Arc::new(name), tx);
        ADDR.with(|cell| *cell.borrow_mut() = Some(arb.clone()));
        STORAGE.with(|cell| cell.borrow_mut().clear());

        (
            arb,
            ArbiterController {
                rx,
                sys: None,
                stop: None,
            },
        )
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
        let _ = self.0.sender.try_send(ArbiterCommand::Stop);
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

        let builder = if sys.config().stack_size > 0 {
            thread::Builder::new()
                .name(name)
                .stack_size(sys.config().stack_size)
        } else {
            thread::Builder::new().name(name)
        };

        let name = name2.clone();
        let sys_id = sys.id();
        let (arb_hnd_tx, arb_hnd_rx) = oneshot::channel();

        let handle = builder
            .spawn(move || {
                let name3 = name2.clone();
                log::info!("Starting {name3:?} arbiter");

                let sys2 = sys.clone();
                let (stop, stop_rx) = oneshot::channel();
                STORAGE.with(|cell| cell.borrow_mut().clear());

                crate::driver::block_on(config.runner.as_ref(), async move {
                    let arb = Arbiter::with_sender(sys_id.0, id, name2, arb_tx);
                    sys.register_arbiter(arb.clone());
                    arb_hnd_tx
                        .send(arb.clone())
                        .expect("Controller thread has gone");

                    // start arbiter controller
                    crate::spawn(
                        ArbiterController {
                            sys: None,
                            stop: Some(stop),
                            rx: arb_rx,
                        }
                        .run(sys),
                    );
                    ADDR.with(|cell| *cell.borrow_mut() = Some(arb.clone()));

                    // run loop
                    let _ = stop_rx.await;

                    // mark as not running
                    arb.0.running.store(false, Ordering::Relaxed);
                });

                // unregister arbiter
                sys2.unregister_arbiter(Id(id));

                remove_all_items();

                log::info!("Arbiter {name3:?} has been stopped");
            })
            .unwrap_or_else(|err| {
                panic!("Cannot spawn an arbiter's thread {name:?}: {err:?}")
            });

        let arb = arb_hnd_rx.recv().expect("Could not start new arbiter");
        *arb.0.thread_handle.lock() = Some(handle);
        arb
    }

    fn with_sender(
        sys_id: usize,
        id: usize,
        name: Arc<String>,
        sender: Sender<ArbiterCommand>,
    ) -> Self {
        #[cfg(feature = "tokio")]
        let hnd = { Handle::new(sender.clone()) };

        #[cfg(feature = "compio")]
        let hnd = { Handle::new(sender.clone()) };

        #[cfg(all(not(feature = "compio"), not(feature = "tokio")))]
        let hnd = { Handle::current() };

        Self(Arc::new(ArbiterInner {
            id,
            sys_id,
            name,
            sender,
            hnd: Some(hnd),
            thread_handle: Mutex::new(None),
            running: AtomicBool::new(true),
            #[cfg(target_os = "linux")]
            #[allow(clippy::cast_possible_truncation)]
            tid: unsafe { libc::syscall(libc::SYS_gettid) } as i32,
        }))
    }

    /// Id of the arbiter
    pub fn id(&self) -> Id {
        Id(self.0.id)
    }

    #[cfg(target_os = "linux")]
    /// TID of the arbiter
    pub(crate) fn tid(&self) -> i32 {
        self.0.tid
    }

    /// Name of the arbiter
    pub fn name(&self) -> &str {
        self.0.name.as_ref()
    }

    #[inline]
    /// Handle to a runtime
    pub fn handle(&self) -> &Handle {
        self.0.hnd.as_ref().unwrap()
    }

    #[inline]
    /// Check if arbiter is running
    pub fn is_running(&self) -> bool {
        self.0.running.load(Ordering::Relaxed)
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
        if let Some(thread_handle) = self.0.thread_handle.lock().take() {
            thread_handle.join()
        } else {
            Ok(())
        }
    }
}

impl Eq for Arbiter {}

impl PartialEq for Arbiter {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id && self.0.sys_id == other.0.sys_id
    }
}

pub(crate) struct ArbiterController {
    sys: Option<System>,
    rx: Receiver<ArbiterCommand>,
    stop: Option<oneshot::Sender<i32>>,
}

impl Drop for ArbiterController {
    fn drop(&mut self) {
        if thread::panicking() {
            if let Some(sys) = self.sys.take()
                && sys.stop_on_panic()
            {
                eprintln!("Panic in Arbiter thread, shutting down system.");
                sys.stop_with_code(1);
            } else {
                eprintln!("Panic in Arbiter thread.");
            }
        }
    }
}

impl ArbiterController {
    pub(super) async fn run(mut self, sys: System) {
        self.sys = Some(sys);
        loop {
            match self.rx.recv().await {
                Ok(ArbiterCommand::Stop) => {
                    if let Some(stop) = self.stop.take() {
                        let _ = stop.send(0);
                    }
                }
                Ok(ArbiterCommand::Execute(fut)) => {
                    crate::spawn(fut);
                }
                Err(_) => break,
            }
        }
    }
}

/// Set item to current runtime's storage
pub fn set_item<T: 'static>(item: T) {
    STORAGE.with(move |cell| cell.borrow_mut().insert(TypeId::of::<T>(), Box::new(item)));
}

/// Get a reference to a type previously inserted on this runtime's storage
pub fn get_item<T: Clone + 'static>() -> Option<T> {
    STORAGE.with(move |cell| {
        cell.borrow()
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
            .cloned()
    })
}

/// Get a reference to a type or create new if it doesnt exists
pub fn with_item<T: Default + 'static, F, R>(f: F) -> R
where
    F: FnOnce(&T) -> R,
{
    STORAGE.with(move |cell| {
        let mut st = cell.borrow_mut();
        if let Some(boxed) = st.get(&TypeId::of::<T>()) {
            f(boxed.downcast_ref().unwrap())
        } else {
            let item = T::default();
            let result = f(&item);
            st.insert(TypeId::of::<T>(), Box::new(item));
            result
        }
    })
}

/// Remove all items from storage.
pub fn remove_all_items() {
    STORAGE.with(move |cell| {
        loop {
            let mut items = cell.borrow_mut();
            let Some(item) = items.drain().next() else {
                break;
            };
            drop(items);
            drop(item);
        }
    });
    System::remove_current();
}
