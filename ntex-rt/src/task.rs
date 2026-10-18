use std::{future::Future, sync::Arc, sync::atomic::AtomicUsize, sync::atomic::Ordering};

// The Callbacks static holds a pointer to the global callbacks. It is protected by
// the STATE static which determines whether `CBS` has been initialized yet.
static mut CBS: Option<Arc<dyn CallbacksApi>> = None;

static STATE: AtomicUsize = AtomicUsize::new(0);

// There are three different states that we care about: the callback's
// uninitialized, the callback's initializing (set_cbs's been called but
// CBS hasn't actually been set yet), or the callbacks's active.
const UNINITIALIZED: usize = 0;
const INITIALIZING: usize = 1;
const INITIALIZED: usize = 2;

trait CallbacksApi {
    fn before(&self) -> Option<*const ()>;
    fn enter(&self, _: *const ()) -> *const ();
    fn exit(&self, _: *const ());
    fn after(&self, _: *const ());
}

#[allow(clippy::struct_field_names)]
struct Callbacks<A, B, C, D> {
    f_before: A,
    f_enter: B,
    f_exit: C,
    f_after: D,
}

impl<A, B, C, D> CallbacksApi for Callbacks<A, B, C, D>
where
    A: Fn() -> Option<*const ()> + 'static,
    B: Fn(*const ()) -> *const () + 'static,
    C: Fn(*const ()) + 'static,
    D: Fn(*const ()) + 'static,
{
    fn before(&self) -> Option<*const ()> {
        (self.f_before)()
    }
    fn enter(&self, d: *const ()) -> *const () {
        (self.f_enter)(d)
    }
    fn exit(&self, d: *const ()) {
        (self.f_exit)(d);
    }
    fn after(&self, d: *const ()) {
        (self.f_after)(d);
    }
}

pub(crate) struct Data {
    cb: &'static dyn CallbacksApi,
    ptr: *const (),
}

impl Data {
    #[allow(clippy::if_not_else)]
    pub(crate) fn load() -> Option<Data> {
        // `Acquire` pairs with the `Release` store in `set_cbs`, so `CBS` is visible
        let cb = if STATE.load(Ordering::Acquire) != INITIALIZED {
            None
        } else {
            #[allow(static_mut_refs)]
            unsafe {
                Some(CBS.as_ref().map(AsRef::as_ref).unwrap())
            }
        };

        if let Some(cb) = cb
            && let Some(ptr) = cb.before()
        {
            return Some(Data { cb, ptr });
        }
        None
    }

    pub(crate) fn run<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        struct Exit(&'static dyn CallbacksApi, *const ());

        impl Drop for Exit {
            fn drop(&mut self) {
                self.0.exit(self.1);
            }
        }

        // `exit` also runs if `f` panics, so the context set by `enter` is restored
        let _exit = Exit(self.cb, self.cb.enter(self.ptr));
        f()
    }
}

/// Wraps a future so that task callbacks, if registered, run around each poll.
///
/// Always returns the same future type, so spawning code is generated once
/// per future rather than once per callback mode.
pub(crate) fn wrap<F: Future>(fut: F) -> impl Future<Output = F::Output> {
    let mut data = Data::load();
    async move {
        let mut f = std::pin::pin!(fut);
        std::future::poll_fn(|cx| match data.as_mut() {
            Some(data) => data.run(|| f.as_mut().poll(cx)),
            None => f.as_mut().poll(cx),
        })
        .await
    }
}

#[cfg(any(feature = "tokio", feature = "compio"))]
pub(crate) use remote::RemoteJoin;

#[cfg(any(feature = "tokio", feature = "compio"))]
mod remote {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};
    use std::{future::Future, future::poll_fn, pin::Pin, pin::pin, sync::Arc};

    use async_channel::Sender;
    use atomic_waker::AtomicWaker;

    use crate::arbiter::ArbiterCommand;

    #[derive(Debug, Default)]
    struct Cancel {
        cancelled: AtomicBool,
        waker: AtomicWaker,
    }

    /// A task spawned on an arbiter from any thread, cancelling it drops the
    /// future on the arbiter thread.
    #[derive(Debug)]
    pub(crate) struct RemoteJoin<T> {
        rx: oneshot::AsyncReceiver<T>,
        cancel: Arc<Cancel>,
    }

    impl<T: Send + 'static> RemoteJoin<T> {
        pub(crate) fn spawn<F>(sender: &Sender<ArbiterCommand>, future: F) -> Self
        where
            F: Future<Output = T> + Send + 'static,
        {
            let (tx, rx) = oneshot::async_channel();
            let cancel = Arc::new(Cancel::default());
            let state = cancel.clone();
            let _ = sender.try_send(ArbiterCommand::Execute(Box::pin(async move {
                let mut fut = pin!(future);
                let res = poll_fn(|cx| {
                    state.waker.register(cx.waker());
                    if state.cancelled.load(Ordering::Acquire) {
                        Poll::Ready(None)
                    } else {
                        fut.as_mut().poll(cx).map(Some)
                    }
                })
                .await;
                if let Some(res) = res {
                    let _ = tx.send(res);
                }
            })));
            Self { rx, cancel }
        }
    }

    impl<T> RemoteJoin<T> {
        /// Cancels the task, a task that did not start never runs.
        pub(crate) fn cancel(self) {
            self.cancel.cancelled.store(true, Ordering::Release);
            self.cancel.waker.wake();
        }

        pub(crate) fn is_finished(&self) -> bool {
            self.rx.is_closed()
        }
    }

    impl<T> Future for RemoteJoin<T> {
        type Output = Result<T, oneshot::RecvError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.rx).poll(cx)
        }
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        self.cb.after(self.ptr);
    }
}

/// # Safety
///
/// The user must ensure that the pointer returned by `before` has a `'static` lifetime.
/// This pointer will be owned by the spawned task for the duration of that task, and
/// ownership will be returned to the user at the end of the task via `after`.
/// The pointer remains opaque to the runtime.
///
/// Does nothing if task callbacks have already been set, use
/// [`task_opt_callbacks`] to check whether they were set.
pub unsafe fn task_callbacks<FBefore, FEnter, FExit, FAfter>(
    f_before: FBefore,
    f_enter: FEnter,
    f_exit: FExit,
    f_after: FAfter,
) where
    FBefore: Fn() -> Option<*const ()> + 'static + Sync,
    FEnter: Fn(*const ()) -> *const () + 'static + Sync,
    FExit: Fn(*const ()) + 'static + Sync,
    FAfter: Fn(*const ()) + 'static + Sync,
{
    let new = Arc::new(Callbacks {
        f_before,
        f_enter,
        f_exit,
        f_after,
    });
    let _ = set_cbs(new);
}

/// # Safety
///
/// The user must ensure that the pointer returned by `before` has a `'static` lifetime.
/// This pointer will be owned by the spawned task for the duration of that task, and
/// ownership will be returned to the user at the end of the task via `after`.
/// The pointer remains opaque to the runtime.
///
/// Returns false if task callbacks have already been set.
pub unsafe fn task_opt_callbacks<FBefore, FEnter, FExit, FAfter>(
    f_before: FBefore,
    f_enter: FEnter,
    f_exit: FExit,
    f_after: FAfter,
) -> bool
where
    FBefore: Fn() -> Option<*const ()> + Sync + 'static,
    FEnter: Fn(*const ()) -> *const () + Sync + 'static,
    FExit: Fn(*const ()) + Sync + 'static,
    FAfter: Fn(*const ()) + Sync + 'static,
{
    let new = Arc::new(Callbacks {
        f_before,
        f_enter,
        f_exit,
        f_after,
    });
    set_cbs(new).is_ok()
}

fn set_cbs(cbs: Arc<dyn CallbacksApi>) -> Result<(), ()> {
    match STATE.compare_exchange(
        UNINITIALIZED,
        INITIALIZING,
        Ordering::Acquire,
        Ordering::Relaxed,
    ) {
        Ok(UNINITIALIZED) => {
            unsafe {
                CBS = Some(cbs);
            }
            STATE.store(INITIALIZED, Ordering::Release);
            Ok(())
        }
        Err(INITIALIZING) => {
            while STATE.load(Ordering::Relaxed) == INITIALIZING {
                std::hint::spin_loop();
            }
            Err(())
        }
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_callbacks_set_by_other_thread() {
        let hnd = std::thread::spawn(|| unsafe {
            task_opt_callbacks(|| Some(std::ptr::null()), |p| p, |_| {}, |_| {})
        });
        // observes callbacks set by another thread without synchronization
        while Data::load().is_none() {
            std::hint::spin_loop();
        }
        assert!(hnd.join().unwrap());
    }

    #[test]
    fn exit_runs_when_poll_panics() {
        use std::{cell::Cell, panic, pin::pin, task::Context, task::Waker};

        // callbacks can be set once per process, so check them in a child process
        const CHILD: &str = "NTEX_RT_TASK_EXIT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "task::tests::exit_runs_when_poll_panics"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
            return;
        }

        thread_local! {
            static DEPTH: Cell<isize> = const { Cell::new(0) };
            static AFTER: Cell<usize> = const { Cell::new(0) };
        }
        assert!(unsafe {
            task_opt_callbacks(
                || Some(std::ptr::null()),
                |p| {
                    DEPTH.set(DEPTH.get() + 1);
                    p
                },
                |_| DEPTH.set(DEPTH.get() - 1),
                |_| AFTER.set(AFTER.get() + 1),
            )
        });

        let mut fut = Box::pin(wrap(async { panic!("poll panic") }));
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _ = fut.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        }));
        assert!(res.is_err());
        assert_eq!(DEPTH.get(), 0);
        drop(fut);
        assert_eq!(AFTER.get(), 1);

        let mut fut = pin!(wrap(async { 1 }));
        let res = fut.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert_eq!(res, std::task::Poll::Ready(1));
        assert_eq!(DEPTH.get(), 0);
    }

    #[test]
    fn callbacks_set_once_and_wrap_spawned_tasks() {
        use std::{cell::Cell, sync::atomic::AtomicUsize};

        use crate::{System, testing::TestRunner};

        const CHILD: &str = "NTEX_RT_TASK_CALLBACKS_CHILD";
        static DATA: u8 = 0;
        static CTX: u8 = 0;
        static BEFORE: AtomicUsize = AtomicUsize::new(0);
        static AFTER: AtomicUsize = AtomicUsize::new(0);
        thread_local! {
            static TRACK: Cell<bool> = const { Cell::new(false) };
            static INSIDE: Cell<bool> = const { Cell::new(false) };
        }
        fn data() -> *const () {
            (&raw const DATA).cast()
        }
        fn ctx() -> *const () {
            (&raw const CTX).cast()
        }

        // callbacks can be set once per process, so check them in a child process
        if std::env::var_os(CHILD).is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "task::tests::callbacks_set_once_and_wrap_spawned_tasks",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
            return;
        }

        unsafe {
            task_callbacks(
                || {
                    TRACK.get().then(|| {
                        BEFORE.fetch_add(1, Ordering::Relaxed);
                        data()
                    })
                },
                |p| {
                    assert_eq!(p, data());
                    INSIDE.set(true);
                    ctx()
                },
                |p| {
                    // receives the value returned by `enter`
                    assert_eq!(p, ctx());
                    INSIDE.set(false);
                },
                |p| {
                    assert_eq!(p, data());
                    AFTER.fetch_add(1, Ordering::Relaxed);
                },
            );
        }
        // callbacks are set once, later registrations are ignored
        unsafe {
            task_callbacks(
                || unreachable!(),
                |_| unreachable!(),
                |_| unreachable!(),
                |_| unreachable!(),
            );
            assert!(!task_opt_callbacks(
                || unreachable!(),
                |_| unreachable!(),
                |_| unreachable!(),
                |_| unreachable!(),
            ));
        }

        System::new("callbacks", TestRunner).block_on(async {
            TRACK.set(true);
            let hnd = crate::spawn(async { INSIDE.get() });
            TRACK.set(false);
            assert_eq!(BEFORE.load(Ordering::Relaxed), 1);
            // polled with the context set by `enter`
            assert!(hnd.await.unwrap());
            assert!(!INSIDE.get());
            assert_eq!(AFTER.load(Ordering::Relaxed), 1);

            // `before` returned `None`, the task runs without callbacks
            assert!(!crate::spawn(async { INSIDE.get() }).await.unwrap());
            assert_eq!(BEFORE.load(Ordering::Relaxed), 1);
            assert_eq!(AFTER.load(Ordering::Relaxed), 1);
        });
    }
}
