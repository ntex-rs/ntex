#![allow(clippy::missing_panics_doc)]
use std::sync::{Arc, atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering};
use std::{any::Any, any::TypeId, cell::RefCell, fmt, mem, panic, pin::Pin, rc::Rc, thread};

use async_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;

use crate::metrics::{LatencyMetrics, LatencyStats, start_latency_sampler};
use crate::{Handle, HashMap, Id, System};

thread_local!(
    static ADDR: RefCell<Option<Arbiter>> = const { RefCell::new(None) };
    static STORAGE: RefCell<HashMap<TypeId, Rc<dyn Any>>> = RefCell::new(HashMap::default());
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
    pub(crate) latency: Arc<LatencyStats>,
    #[cfg(target_os = "linux")]
    tid: i32,
    #[cfg(windows)]
    thread: Option<crate::watchdog::ThreadHandle>,
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
        let id = COUNT.fetch_add(1, Ordering::Relaxed);
        Arbiter::start(id, format!("{}:arb:{}", System::current().name(), id), None)
    }

    /// Starts an arbiter on a new thread with the specified name.
    pub fn with_name(name: String) -> Arbiter {
        Arbiter::start(COUNT.fetch_add(1, Ordering::Relaxed), name, None)
    }

    /// Starts an arbiter on a new thread with the specified name.
    ///
    /// `init` runs on the new thread before the arbiter's runtime is created,
    /// for example to set the thread's CPU affinity, so that the runtime's
    /// memory is allocated on the NUMA node of that CPU.
    pub fn with_init<F>(name: String, init: F) -> Arbiter
    where
        F: FnOnce() + Send + 'static,
    {
        Arbiter::start(
            COUNT.fetch_add(1, Ordering::Relaxed),
            name,
            Some(Box::new(init)),
        )
    }

    fn start(id: usize, name: String, init: Option<Box<dyn FnOnce() + Send>>) -> Arbiter {
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
                if let Some(init) = init {
                    init();
                }
                let name3 = name2.clone();
                log::info!("Starting {name3:?} arbiter");

                let sys2 = sys.clone();
                let (stop, stop_rx) = oneshot::channel();
                clear_storage();

                // `block_on` finalizes the arbiter once its event loop exits
                let result = crate::driver::block_on(config.runner.as_ref(), async move {
                    let arb = Arbiter::with_sender(sys_id.0, id, name2, arb_tx, Arc::default());
                    sys.register_arbiter(arb.clone());
                    // `Arbiter::latency()` is available once the arbiter is returned
                    start_latency_sampler(&arb.0.latency, config.latency_interval);
                    arb_hnd_tx
                        .send(arb.clone())
                        .expect("Controller thread has gone");

                    // start arbiter controller
                    crate::spawn_detached(
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
                });

                let cleanup = || {
                    // unregister arbiter
                    sys2.unregister_arbiter(Id(id));
                    // skipped by `block_on` if the event loop panicked
                    run_shutdown_callbacks();
                    unsafe {
                        remove_all_items();
                    }
                };
                if result.is_err() {
                    crate::abort_on_panic(cleanup);
                } else {
                    cleanup();
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
            latency: Arc::default(),
            #[cfg(target_os = "linux")]
            #[allow(clippy::cast_possible_truncation)]
            tid: unsafe { libc::syscall(libc::SYS_gettid) } as i32,
            #[cfg(windows)]
            thread: crate::watchdog::ThreadHandle::current(),
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

    #[cfg(windows)]
    /// Handle of the arbiter thread
    pub(crate) fn thread(&self) -> Option<&crate::watchdog::ThreadHandle> {
        self.0.thread.as_ref()
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

    /// Returns the arbiter's scheduling latency.
    ///
    /// Returns `None` if latency sampling is disabled, see
    /// [`Builder::latency_interval`](crate::Builder::latency_interval).
    pub fn latency(&self) -> Option<LatencyMetrics> {
        self.0.latency.metrics()
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
                st.insert(TypeId::of::<T>(), Rc::new(val.clone()));
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
    /// destroyed, `f` is dropped without running. If `f` panics after the
    /// event loop panicked, the process is aborted.
    pub fn on_shutdown<F>(f: F)
    where
        F: FnOnce() + 'static,
    {
        let f: Box<dyn FnOnce()> = Box::new(f);
        let _ = ON_SHUTDOWN.try_with(move |cell| cell.borrow_mut().push(f));
    }

    #[must_use]
    /// Adds a callback to run after the arbiter stops.
    ///
    /// If `f` panics after the event loop panicked, the process is aborted.
    pub fn on_stop<F>(self, f: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.0.on_stop.lock().push(Box::new(f));
        self
    }

    /// Returns the arbiter of the current thread, if any.
    pub(crate) fn try_current() -> Option<Arbiter> {
        ADDR.try_with(|cell| cell.borrow().clone()).ok().flatten()
    }

    /// Marks the arbiter as stopped and runs its `on_stop` callbacks,
    /// called once its event loop has exited.
    pub(crate) fn finalize(&self) {
        self.0.running.store(false, Ordering::Relaxed);
        let on_stop = mem::take(&mut *self.0.on_stop.lock());
        // a panic while unwinding aborts the process
        for f in on_stop {
            f();
        }
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
                    crate::spawn_detached(fut);
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
    let item: Rc<dyn Any> = Rc::new(item);
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
    // `f` holds its own reference, so the value stays alive even if `f`
    // replaces or removes it
    let val = STORAGE
        .try_with(|cell| {
            let existing = cell.borrow().get(&TypeId::of::<T>()).cloned();
            existing.unwrap_or_else(|| {
                let val: Rc<dyn Any> = Rc::new(T::default());
                cell.borrow_mut().insert(TypeId::of::<T>(), val.clone());
                val
            })
        })
        .ok();
    match val {
        Some(val) => f(val.downcast_ref::<T>().unwrap()),
        None => f(&T::default()),
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
    use std::cell::Cell;

    use super::*;
    use crate::testing::TestRunner;

    #[derive(Clone, Default)]
    struct Value(usize);

    #[test]
    #[should_panic(expected = "Arbiter is not running")]
    fn current_without_arbiter() {
        let _ = Arbiter::current();
    }

    #[test]
    fn arbiter_with_init() {
        thread_local! {
            static INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }

        System::new("arb-init", TestRunner).block_on(async {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut arb = Arbiter::with_init("init".to_string(), move || {
                INIT.with(|init| init.set(true));
                let _ = tx.send(thread::current().name().map(str::to_string));
            });
            assert_eq!(rx.recv().unwrap().as_deref(), Some("init"));

            // `init` ran on the arbiter's thread
            let (tx, rx) = oneshot::async_channel();
            arb.handle()
                .spawn(async move {
                    let _ = tx.send(INIT.with(std::cell::Cell::get));
                })
                .detach();
            assert!(rx.await.unwrap());
            assert!(!INIT.with(std::cell::Cell::get));

            arb.stop();
            arb.join().unwrap();
        });
    }

    #[test]
    fn arbiter_api() {
        System::new("arb-test", TestRunner).block_on(async {
            let sys = System::current();
            let cur = Arbiter::current();
            assert_eq!(cur.name(), "arb-test");
            assert_eq!(cur, sys.arbiter());
            assert!(cur.is_running());
            assert_eq!(format!("{cur:?}"), "Arbiter(\"arb-test\")");
            // the primary arbiter does not own a thread
            assert!(cur.clone().join().is_ok());

            let stopped = Arc::new(AtomicUsize::new(0));
            let stopped2 = stopped.clone();
            let mut arb = Arbiter::with_name("named".to_string()).on_stop(move || {
                stopped2.fetch_add(1, Ordering::Relaxed);
            });
            assert_eq!(arb.name(), "named");
            assert!(arb.is_running());
            assert_ne!(arb, cur);
            sys.list_arbiters(|arbs| assert!(arbs.contains(&arb)));

            // shutdown callbacks run on the arbiter's thread
            let (reg_tx, reg_rx) = oneshot::async_channel();
            let (tx, rx) = oneshot::channel();
            arb.handle()
                .spawn(async move {
                    let name = Arbiter::current().name().to_string();
                    Arbiter::on_shutdown(move || {
                        let _ = tx.send(name);
                    });
                    let _ = reg_tx.send(());
                })
                .detach();
            reg_rx.await.unwrap();

            arb.stop();
            arb.join().unwrap();
            assert!(!arb.is_running());
            assert_eq!(stopped.load(Ordering::Relaxed), 1);
            assert_eq!(rx.recv().unwrap(), "named");
            sys.list_arbiters(|arbs| assert!(!arbs.contains(&arb)));

            let mut arb = Arbiter::default();
            assert!(arb.name().starts_with("arb-test:arb:"));
            arb.stop();
            arb.join().unwrap();
        });
    }

    #[test]
    fn handle_spawn_cancel_before_run() {
        System::new("cancel-before", TestRunner).block_on(async {
            let executed = Arc::new(AtomicUsize::new(0));
            let executed2 = executed.clone();
            let hnd = Handle::current();
            hnd.spawn(async move {
                executed2.fetch_add(1, Ordering::Relaxed);
            })
            .cancel();
            // commands run in order, the cancelled one was processed first
            hnd.spawn(async {}).await.unwrap();
            hnd.spawn(async {}).await.unwrap();
            assert_eq!(executed.load(Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn handle_spawn_cancel_while_running() {
        struct Token(Option<oneshot::Sender<()>>);
        impl Drop for Token {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }

        System::new("cancel-running", TestRunner).block_on(async {
            let mut arb = Arbiter::new();
            let resumed = Arc::new(AtomicBool::new(false));
            let resumed2 = resumed.clone();
            let (started_tx, started_rx) = oneshot::async_channel();
            let (drop_tx, drop_rx) = oneshot::async_channel();
            let hnd = arb.handle().spawn(async move {
                let _token = Token(Some(drop_tx));
                let _ = started_tx.send(());
                std::future::pending::<()>().await;
                resumed2.store(true, Ordering::Relaxed);
            });
            started_rx.await.unwrap();
            hnd.cancel();
            // the future is dropped on the arbiter thread
            let mut drop_rx = std::pin::pin!(drop_rx);
            let mut timeout =
                std::pin::pin!(futures_timer::Delay::new(std::time::Duration::from_secs(5)));
            std::future::poll_fn(|cx| {
                if drop_rx.as_mut().poll(cx).is_ready() {
                    std::task::Poll::Ready(())
                } else if timeout.as_mut().poll(cx).is_ready() {
                    panic!("cancelled task is not dropped");
                } else {
                    std::task::Poll::Pending
                }
            })
            .await;
            assert!(!resumed.load(Ordering::Relaxed));

            // detached tasks keep running
            let (tx, rx) = oneshot::async_channel();
            arb.handle()
                .spawn(async move {
                    let _ = tx.send(());
                })
                .detach();
            rx.await.unwrap();

            arb.stop();
            arb.join().unwrap();
        });
    }

    #[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
    #[test]
    fn panicking_arbiter() {
        /// Returns a panic of the event loop as an error, like the ntex-net
        /// runners.
        struct CatchRunner;

        impl crate::Runner for CatchRunner {
            fn block_on(
                &self,
                fut: crate::BlockFuture,
            ) -> Result<(), Box<dyn std::any::Any + Send>> {
                panic::catch_unwind(panic::AssertUnwindSafe(|| TestRunner.block_on(fut)))?
            }
        }

        fn check(runner: impl crate::Runner) {
            System::new("arb-panic", runner).block_on(async {
                let sys = System::current();
                let mut arb = Arbiter::with_name("panics".to_string());
                let (tx, rx) = oneshot::channel();
                arb.handle()
                    .spawn(async move {
                        Arbiter::on_shutdown(move || {
                            let _ = tx.send(());
                        });
                        panic!("arbiter boom");
                    })
                    .detach();

                // the panic of the event loop is returned by `join`
                let err = arb.join().unwrap_err();
                assert_eq!(err.downcast_ref::<&str>(), Some(&"arbiter boom"));
                // the arbiter is unregistered and its shutdown callbacks ran
                assert!(rx.recv().is_ok());
                assert!(!arb.is_running());
                sys.list_arbiters(|arbs| assert!(!arbs.contains(&arb)));
            });
        }

        check(CatchRunner);
        // the panic unwinds through the runner
        check(TestRunner);
    }

    #[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
    #[test]
    #[cfg_attr(miri, ignore = "spawns a process")]
    fn panicking_arbiter_cleanup_aborts() {
        const CHILD: &str = "NTEX_RT_ARBITER_CLEANUP_CHILD";

        struct CatchRunner;

        impl crate::Runner for CatchRunner {
            fn block_on(
                &self,
                fut: crate::BlockFuture,
            ) -> Result<(), Box<dyn std::any::Any + Send>> {
                panic::catch_unwind(panic::AssertUnwindSafe(|| TestRunner.block_on(fut)))?
            }
        }

        if let Ok(case) = std::env::var(CHILD) {
            System::new("arb-panic", CatchRunner).block_on(async move {
                let mut arb = Arbiter::with_name("panics".to_string());
                let on_stop = case == "on_stop";
                arb.handle()
                    .spawn(async move {
                        if on_stop {
                            let _ = Arbiter::current().on_stop(|| panic!("cleanup panic"));
                        } else {
                            Arbiter::on_shutdown(|| panic!("cleanup panic"));
                        }
                        panic!("arbiter boom");
                    })
                    .detach();
                let _ = arb.join();
            });
            return;
        }

        for case in ["on_stop", "on_shutdown"] {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "arbiter::tests::panicking_arbiter_cleanup_aborts",
                    "--exact",
                ])
                .args(["--nocapture", "--test-threads=1"])
                .env(CHILD, case)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success(), "{case}: {stderr}");
            // a normal test failure exits with 101, an abort does not
            assert_ne!(out.status.code(), Some(101), "{case}: {stderr}");
            assert!(stderr.contains("arbiter boom"), "{case}: {stderr}");
            assert!(stderr.contains("cleanup panic"), "{case}: {stderr}");
        }
    }

    #[test]
    fn arbiter_values() {
        System::new("test", TestRunner).block_on(async {
            assert_eq!(Arbiter::get_value(|| Value(1)).0, 1);
            assert_eq!(Arbiter::get_value(|| Value(2)).0, 1);
        });
    }

    #[test]
    fn on_shutdown_runs_after_block_on() {
        let called = Rc::new(Cell::new(0));
        let called2 = called.clone();
        System::new("test", TestRunner).block_on(async move {
            Arbiter::on_shutdown(move || {
                called2.set(called2.get() + 1);
                // registered by a callback, runs in the same shutdown
                Arbiter::on_shutdown(move || called2.set(called2.get() + 1));
            });
        });
        assert_eq!(called.get(), 2);
    }

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
