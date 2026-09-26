#![allow(clippy::missing_panics_doc)]
use std::sync::{Arc, atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering};
use std::{any::Any, any::TypeId, cell::RefCell, fmt, mem, panic, pin::Pin, thread};

use async_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;

use crate::{Handle, HashMap, Id, System};

thread_local!(
    static ADDR: RefCell<Option<Arbiter>> = const { RefCell::new(None) };
    static STORAGE: RefCell<HashMap<TypeId, Box<dyn Any>>> = RefCell::new(HashMap::default());
    static ON_SHUTDOWN: RefCell<Vec<Box<dyn FnOnce()>>> = const { RefCell::new(Vec::new()) };
);

pub(super) static COUNT: AtomicUsize = AtomicUsize::new(99);

pub(super) enum ArbiterCommand {
    Stop,
    #[allow(dead_code)]
    Execute(Pin<Box<dyn Future<Output = ()> + Send>>),
}

/// An asynchronous execution environment running on one OS thread.
///
/// Creating an arbiter starts a thread with its own local event loop. Futures
/// spawned on that event loop are not required to implement `Send`.
pub struct Arbiter(pub(crate) Arc<ArbiterInner>);

type OnCloseStorage = Arc<Mutex<Vec<Box<dyn Fn() + Send + Sync>>>>;

pub(crate) struct ArbiterInner {
    id: usize,
    name: Arc<String>,
    sys_id: usize,
    hnd: Option<Handle>,
    pub(crate) sender: Sender<ArbiterCommand>,
    thread_handle: Mutex<Option<thread::JoinHandle<()>>>,
    on_stop: OnCloseStorage,
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
        let arb = Arbiter::with_sender(id, aid, Arc::new(name), tx, Arc::default());
        ADDR.with(|cell| *cell.borrow_mut() = Some(arb.clone()));
        clear_storage();

        (
            arb,
            ArbiterController {
                rx,
                sys: None,
                stop: None,
            },
        )
    }

    /// Returns the arbiter running on the current thread.
    ///
    /// # Panics
    ///
    /// Panics if no arbiter is running on the current thread.
    pub fn current() -> Arbiter {
        ADDR.with(|cell| match *cell.borrow() {
            Some(ref addr) => addr.clone(),
            None => panic!("Arbiter is not running"),
        })
    }

    /// Requests that the arbiter stop its event loop.
    pub fn stop(&self) {
        let _ = self.0.sender.try_send(ArbiterCommand::Stop);
    }

    /// Starts an arbiter on a new thread with an automatically generated name.
    pub fn new() -> Arbiter {
        let id = COUNT.load(Ordering::Relaxed) + 1;
        Arbiter::with_name(format!("{}:arb:{}", System::current().name(), id))
    }

    /// Starts an arbiter on a new thread with the specified name.
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
                clear_storage();

                let on_stop = Arc::new(Mutex::new(Vec::new()));
                let on_stop2 = on_stop.clone();

                let result = crate::driver::block_on(config.runner.as_ref(), async move {
                    let arb = Arbiter::with_sender(sys_id.0, id, name2, arb_tx, on_stop);
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

                let on_stop = mem::take(&mut *on_stop2.lock());
                for f in on_stop {
                    f();
                }

                // unregister arbiter
                sys2.unregister_arbiter(Id(id));
                // skipped by `block_on` if the event loop panicked
                run_shutdown_callbacks();
                unsafe {
                    remove_all_items();
                }

                if let Err(e) = result {
                    log::error!("Arbiter {name3:?} has panicked.");
                    panic::resume_unwind(e);
                }
                log::info!("Arbiter {name3:?} has stopped");
            })
            .unwrap_or_else(|err| panic!("Cannot spawn an arbiter's thread {name:?}: {err:?}"));

        let arb = arb_hnd_rx.recv().expect("Could not start new arbiter");
        *arb.0.thread_handle.lock() = Some(handle);
        arb
    }

    fn with_sender(
        sys_id: usize,
        id: usize,
        name: Arc<String>,
        sender: Sender<ArbiterCommand>,
        on_stop: OnCloseStorage,
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
            on_stop,
            hnd: Some(hnd),
            thread_handle: Mutex::new(None),
            running: AtomicBool::new(true),
            #[cfg(target_os = "linux")]
            #[allow(clippy::cast_possible_truncation)]
            tid: unsafe { libc::syscall(libc::SYS_gettid) } as i32,
        }))
    }

    /// Returns the arbiter identifier.
    pub fn id(&self) -> Id {
        Id(self.0.id)
    }

    #[cfg(target_os = "linux")]
    /// TID of the arbiter
    pub(crate) fn tid(&self) -> i32 {
        self.0.tid
    }

    /// Returns the arbiter name.
    pub fn name(&self) -> &str {
        self.0.name.as_ref()
    }

    #[inline]
    /// Returns a handle to the arbiter's runtime.
    pub fn handle(&self) -> &Handle {
        self.0.hnd.as_ref().unwrap()
    }

    #[inline]
    /// Returns whether the arbiter is running.
    pub fn is_running(&self) -> bool {
        self.0.running.load(Ordering::Relaxed)
    }

    /// Returns a value from thread-local arbiter storage, inserting it if absent.
    ///
    /// If the storage has already been destroyed because the thread is
    /// exiting, the value returned by `f` is not stored.
    pub fn get_value<T, F>(f: F) -> T
    where
        T: Clone + 'static,
        F: FnOnce() -> T,
    {
        let mut f = Some(f);
        STORAGE
            .try_with(|cell| {
                let mut st = cell.borrow_mut();
                if let Some(boxed) = st.get(&TypeId::of::<T>())
                    && let Some(val) = (&**boxed as &(dyn Any + 'static)).downcast_ref::<T>()
                {
                    return val.clone();
                }
                let val = (f.take().unwrap())();
                st.insert(TypeId::of::<T>(), Box::new(val.clone()));
                val
            })
            .unwrap_or_else(|_| (f.take().unwrap())())
    }

    /// Registers a callback to run when the current thread's arbiter shuts down.
    ///
    /// Callbacks run once, in registration order, on the arbiter's thread when
    /// its stop is requested, by [`Arbiter::stop()`] or [`System::stop()`],
    /// while the event loop is still running. An arbiter that ends without a
    /// stop request, such as one driven by
    /// [`SystemRunner::block_on()`](crate::SystemRunner::block_on), runs them
    /// once its event loop has exited instead. A callback registered by
    /// another callback runs in the same shutdown.
    ///
    /// If the thread is exiting and its thread-local storage has already been
    /// destroyed, `f` is dropped without running.
    pub fn on_shutdown<F>(f: F)
    where
        F: FnOnce() + 'static,
    {
        let f: Box<dyn FnOnce()> = Box::new(f);
        let _ = ON_SHUTDOWN.try_with(move |cell| cell.borrow_mut().push(f));
    }

    #[must_use]
    /// Adds a callback to run after the arbiter stops.
    pub fn on_stop<F>(self, f: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.0.on_stop.lock().push(Box::new(f));
        self
    }

    /// Waits for the arbiter's thread to stop.
    ///
    /// This returns immediately for an arbiter that does not own a thread
    /// handle, including the system's primary arbiter.
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

impl ArbiterController {
    pub(super) async fn run(mut self, sys: System) {
        self.sys = Some(sys);
        loop {
            match self.rx.recv().await {
                Ok(ArbiterCommand::Stop) => {
                    // the system arbiter has no `stop`, `System::stop()`
                    // runs its callbacks
                    if let Some(stop) = self.stop.take() {
                        run_shutdown_callbacks();
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

/// Runs the callbacks registered with [`Arbiter::on_shutdown()`], including
/// ones registered while they run.
pub(crate) fn run_shutdown_callbacks() {
    loop {
        let callbacks = ON_SHUTDOWN
            .try_with(|cell| mem::take(&mut *cell.borrow_mut()))
            .unwrap_or_default();
        if callbacks.is_empty() {
            break;
        }
        for f in callbacks {
            f();
        }
    }
}

/// Inserts a value into the current arbiter's thread-local storage.
///
/// If the storage has already been destroyed because the thread is
/// exiting, the value is dropped.
pub fn set_item<T: 'static>(item: T) {
    let item: Box<dyn Any> = Box::new(item);
    let old = STORAGE
        .try_with(move |cell| cell.borrow_mut().insert(TypeId::of::<T>(), item))
        .ok()
        .flatten();
    drop(old);
}

/// Returns a cloned value from the current arbiter's thread-local storage.
///
/// Returns `None` if the storage has already been destroyed because the
/// thread is exiting.
pub fn get_item<T: Clone + 'static>() -> Option<T> {
    STORAGE
        .try_with(move |cell| {
            cell.borrow()
                .get(&TypeId::of::<T>())
                .and_then(|boxed| boxed.downcast_ref())
                .cloned()
        })
        .ok()
        .flatten()
}

/// Provides access to a value in the current arbiter's thread-local storage.
///
/// A default value is inserted if the requested type is not already present.
/// If the storage has already been destroyed because the thread is exiting,
/// `f` receives a temporary default value that is not stored.
pub fn with_item<T: Default + 'static, F, R>(f: F) -> R
where
    F: FnOnce(&T) -> R,
{
    let mut f = Some(f);
    let result = STORAGE.try_with(|cell| {
        // SAFETY: value of T is stored in heap, manipulation
        // with STORAGE are not affected location of T
        let val: &T = unsafe {
            let mut st = cell.borrow_mut();
            if let Some(boxed) = st.get(&TypeId::of::<T>()) {
                std::mem::transmute::<&T, &T>(boxed.downcast_ref::<T>().unwrap())
            } else {
                st.insert(TypeId::of::<T>(), Box::new(T::default()));
                let boxed = st.get(&TypeId::of::<T>()).unwrap();
                std::mem::transmute::<&T, &T>(boxed.downcast_ref::<T>().unwrap())
            }
        };
        (f.take().unwrap())(val)
    });
    match result {
        Ok(res) => res,
        Err(_) => (f.take().unwrap())(&T::default()),
    }
}

#[doc(hidden)]
/// Remove all items from storage.
///
/// # Safety
///
/// All outstanding calls to [`with_item`] must have completed.
pub unsafe fn remove_all_items() {
    clear_storage();
    System::remove_current();
}

/// Removes all items from the storage.
///
/// Each item is dropped outside of the storage borrow, so that its destructor
/// may access the storage. Items it inserts are removed too.
fn clear_storage() {
    let _ = STORAGE.try_with(|cell| {
        loop {
            let mut items = cell.borrow_mut();
            let Some(key) = items.keys().next().copied() else {
                break;
            };
            let item = items.remove(&key);
            drop(items);
            drop(item);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Value(usize);

    fn use_storage() {
        set_item(Value(1));
        let _ = get_item::<Value>();
        with_item::<Value, _, _>(|v| v.0);
        Arbiter::get_value(|| Value(2));
    }

    struct UseOnDrop;

    impl Drop for UseOnDrop {
        fn drop(&mut self) {
            use_storage();
        }
    }

    thread_local!(static HOLD: RefCell<Option<UseOnDrop>> = const { RefCell::new(None) });

    #[test]
    fn storage_access_during_thread_exit() {
        // item stored in STORAGE accesses STORAGE while it is destroyed
        thread::spawn(|| set_item(UseOnDrop)).join().unwrap();

        // other thread local accesses STORAGE, in both destruction orders
        thread::spawn(|| {
            HOLD.with(|h| *h.borrow_mut() = Some(UseOnDrop));
            use_storage();
        })
        .join()
        .unwrap();
        thread::spawn(|| {
            use_storage();
            HOLD.with(|h| *h.borrow_mut() = Some(UseOnDrop));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn with_item_value_outlives_replacement() {
        #[derive(Clone, Default)]
        struct Item(std::rc::Rc<Vec<u8>>);

        thread::spawn(|| {
            set_item(Item(std::rc::Rc::new(vec![1; 64])));
            let len = with_item::<Item, _, _>(|item| {
                // replaces and frees the stored value while `f` holds its clone
                set_item(Item::default());
                unsafe { remove_all_items() };
                item.0.len()
            });
            assert_eq!(len, 64);
            assert!(get_item::<Item>().is_none());
            assert_eq!(with_item::<Item, _, _>(|item| item.0.len()), 0);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn remove_all_items_drops_outside_borrow() {
        struct Item;

        impl Drop for Item {
            fn drop(&mut self) {
                let _ = get_item::<u32>();
                set_item(2u64);
            }
        }

        thread::spawn(|| {
            set_item(Item);
            set_item(1u32);
            unsafe { remove_all_items() };
            assert!(get_item::<u32>().is_none());
            assert!(get_item::<u64>().is_none(), "item inserted by a destructor");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn storage_fallback_values() {
        struct Check;

        impl Drop for Check {
            fn drop(&mut self) {
                set_item(Value(5));
                assert!(get_item::<Value>().is_none());
                assert_eq!(with_item::<Value, _, _>(|v| v.0), 0);
                assert_eq!(Arbiter::get_value(|| Value(3)).0, 3);
                assert_eq!(Arbiter::get_value(|| Value(4)).0, 4);
            }
        }
        thread::spawn(|| set_item(Check)).join().unwrap();
    }
}
