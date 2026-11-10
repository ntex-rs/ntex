use super::*;

/// STATEMENT: A wrapped task exits its callback context even when polling panics.
///
/// HOW:
/// - Install counting callbacks on the test thread and catch a wrapped poll panic.
/// - Check balanced enter/exit state, one after-drop callback, and a normal ready poll.
#[test]
fn exit_runs_when_poll_panics() {
    use std::{panic, pin::pin, task::Waker};

    thread_local! {
        static DEPTH: Cell<isize> = const { Cell::new(0) };
        static AFTER: Cell<usize> = const { Cell::new(0) };
    }
    set_callbacks(Some(TaskCallbacks {
        before: || Some(std::ptr::null()),
        enter: |p| {
            DEPTH.set(DEPTH.get() + 1);
            p
        },
        exit: |_| DEPTH.set(DEPTH.get() - 1),
        after: |_| AFTER.set(AFTER.get() + 1),
    }));

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
    assert_eq!(res, Poll::Ready(1));
    assert_eq!(DEPTH.get(), 0);
    set_callbacks(None);
}

/// STATEMENT: A system's task callbacks factory runs once on the system thread
/// and on each arbiter thread, its callbacks wrap tasks opted in by `before`,
/// and they do not leak into a later system.
///
/// HOW:
/// - Build a system with a callbacks factory, check it ran once on this thread,
///   spawn opted-in and opted-out tasks with `spawn` and `spawn_sendable`, and
///   check context propagation and counts.
/// - Spawn an opted-in task on a new arbiter, check the factory ran on the
///   arbiter thread and the task runs inside the context.
/// - Run a system without callbacks on the same thread and check `before` is not called.
#[test]
fn system_callbacks_wrap_spawned_tasks() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{Arbiter, System, testing::TestRunner};

    static DATA: u8 = 0;
    static CTX: u8 = 0;
    static BEFORE: AtomicUsize = AtomicUsize::new(0);
    static AFTER: AtomicUsize = AtomicUsize::new(0);
    static FACTORY: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static CREATED: Cell<bool> = const { Cell::new(false) };
        static TRACK: Cell<bool> = const { Cell::new(false) };
        static INSIDE: Cell<bool> = const { Cell::new(false) };
    }
    fn data() -> *const () {
        (&raw const DATA).cast()
    }
    fn ctx() -> *const () {
        (&raw const CTX).cast()
    }
    fn tracked<F: Future>(f: impl FnOnce() -> F) -> F {
        TRACK.set(true);
        let fut = f();
        TRACK.set(false);
        fut
    }

    fn factory() -> TaskCallbacks {
        FACTORY.fetch_add(1, Ordering::Relaxed);
        CREATED.set(true);
        TaskCallbacks {
            before: || {
                TRACK.get().then(|| {
                    BEFORE.fetch_add(1, Ordering::Relaxed);
                    data()
                })
            },
            enter: |p| {
                assert_eq!(p, data());
                INSIDE.set(true);
                ctx()
            },
            exit: |p| {
                // receives the value returned by `enter`
                assert_eq!(p, ctx());
                INSIDE.set(false);
            },
            after: |p| {
                assert_eq!(p, data());
                AFTER.fetch_add(1, Ordering::Relaxed);
            },
        }
    }

    System::build()
        .name("callbacks")
        .task_callbacks(factory)
        .build(TestRunner)
        .block_on(async {
            assert!(CREATED.get());
            assert_eq!(FACTORY.load(Ordering::Relaxed), 1);

            let hnd = tracked(|| crate::spawn(async { INSIDE.get() }));
            assert_eq!(BEFORE.load(Ordering::Relaxed), 1);
            // polled with the context set by `enter`
            assert!(hnd.await.unwrap());
            assert!(!INSIDE.get());
            assert_eq!(AFTER.load(Ordering::Relaxed), 1);

            // `before` returned `None`, the task runs without callbacks
            assert!(!crate::spawn(async { INSIDE.get() }).await.unwrap());
            assert_eq!(BEFORE.load(Ordering::Relaxed), 1);
            assert_eq!(AFTER.load(Ordering::Relaxed), 1);

            let result = tracked(|| crate::spawn_sendable(async { INSIDE.get() }));
            assert_eq!(BEFORE.load(Ordering::Relaxed), 2);
            assert!(result.await.unwrap());
            assert!(!INSIDE.get());
            assert_eq!(AFTER.load(Ordering::Relaxed), 2);

            // arbiters inherit the system's callbacks
            let mut arb = Arbiter::new();
            let inside = arb
                .handle()
                .spawn(async {
                    assert!(CREATED.get());
                    // `spawn_sendable` keeps this remote future `Send`
                    tracked(|| crate::spawn_sendable(async { INSIDE.get() }))
                        .await
                        .unwrap()
                })
                .await
                .unwrap();
            assert!(inside);
            assert_eq!(BEFORE.load(Ordering::Relaxed), 3);
            assert_eq!(FACTORY.load(Ordering::Relaxed), 2);
            arb.stop();
            arb.join().unwrap();
        });

    System::new("plain", TestRunner).block_on(async {
        assert!(
            !tracked(|| crate::spawn(async { INSIDE.get() }))
                .await
                .unwrap()
        );
        assert_eq!(BEFORE.load(Ordering::Relaxed), 3);
    });
}
