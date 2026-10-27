use super::*;

/// STATEMENT: Task callbacks published by another thread become visible to readers.
///
/// HOW:
/// - Install callbacks on a spawned thread while loading them on the caller.
/// - Wait for publication before joining and confirm installation succeeded.
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

/// STATEMENT: A wrapped task exits its callback context even when polling panics.
///
/// HOW:
/// - Install counting callbacks in a child process and catch a wrapped poll panic.
/// - Check balanced enter/exit state, one after-drop callback, and a normal ready poll.
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

/// STATEMENT: Task callbacks install once and wrap only tasks opted in by the before callback.
///
/// HOW:
/// - Install callbacks in a child process and verify later registrations are ignored.
/// - Spawn opted-in and opted-out tasks, checking context propagation and callback counts.
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
