use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Poll, Wake};
use std::{future::poll_fn, ptr, sync::Arc};

use super::*;
use crate::Runner;

#[derive(Default)]
struct Counter(AtomicUsize);

impl Counter {
    // A fixed vtable makes identity assertions independent of codegen deduplication.
    fn waker(counter: Arc<Self>) -> Waker {
        unsafe fn clone(data: *const ()) -> RawWaker {
            // SAFETY: `data` comes from `Arc::into_raw` and the source waker keeps a strong count
            // alive.
            unsafe { Arc::increment_strong_count(data.cast::<Counter>()) };
            RawWaker::new(data, &VTABLE)
        }
        unsafe fn wake(data: *const ()) {
            // SAFETY: This consuming wake takes ownership of exactly one raw `Arc` strong count.
            let counter = unsafe { Arc::from_raw(data.cast::<Counter>()) };
            counter.0.fetch_add(1, Ordering::Relaxed);
        }
        unsafe fn wake_by_ref(data: *const ()) {
            // SAFETY: The borrowed waker keeps the `Counter` allocation alive; its counter is
            // atomic.
            unsafe { &*data.cast::<Counter>() }
                .0
                .fetch_add(1, Ordering::Relaxed);
        }
        unsafe fn drop(data: *const ()) {
            // SAFETY: Dropping the waker consumes exactly one strong count created by
            // `Arc::into_raw` or clone.
            std::mem::drop(unsafe { Arc::from_raw(data.cast::<Counter>()) });
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        // SAFETY: The data owns an `Arc<Counter>` count and the vtable balances counts with atomic
        // operations.
        unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(counter).cast(), &VTABLE)) }
    }
}

// Cloning changes both raw parts to those of the inner waker.
fn changing_waker(waker: Waker) -> Waker {
    unsafe fn clone(data: *const ()) -> RawWaker {
        // SAFETY: `data` points to the boxed `Waker` kept alive by the source waker during cloning.
        let waker = ManuallyDrop::new(unsafe { &*data.cast::<Waker>() }.clone());
        RawWaker::new(waker.data(), waker.vtable())
    }
    unsafe fn wake(data: *const ()) {
        // SAFETY: Consuming wake owns the box created for this waker and reclaims it exactly once.
        unsafe { Box::from_raw(data.cast_mut().cast::<Waker>()) }.wake();
    }
    unsafe fn wake_by_ref(data: *const ()) {
        // SAFETY: The borrowed wrapper waker keeps its boxed inner `Waker` alive during this call.
        unsafe { &*data.cast::<Waker>() }.wake_by_ref();
    }
    unsafe fn drop(data: *const ()) {
        // SAFETY: Dropping the wrapper reclaims its uniquely owned box exactly once.
        std::mem::drop(unsafe { Box::from_raw(data.cast_mut().cast::<Waker>()) });
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    // SAFETY: The data owns a boxed `Waker`; the vtable clones or consumes it without
    // unsynchronized mutation.
    unsafe {
        Waker::from_raw(RawWaker::new(
            Box::into_raw(Box::new(waker)).cast(),
            &VTABLE,
        ))
    }
}

/// STATEMENT: Fallback local wakers respect clone identity changes and preserve owned raw parts.
///
/// HOW:
/// - Convert and clone a wrapper whose clone returns a different waker identity.
/// - Check wake forwarding, round-trip raw parts, and final reference counts.
#[test]
fn fallback_clone_parts_and_owned_conversion() {
    let counter = Arc::new(Counter::default());
    {
        let target = Counter::waker(counter.clone());
        let original = changing_waker(target.clone());
        let borrowed = LocalWaker::from_waker(&original);
        assert!(!borrowed.is_native());
        assert!(borrowed.will_wake(&target));
        assert!(!borrowed.will_wake(&original));

        let (data, vtable) = (original.data(), original.vtable());
        let owned = LocalWaker::from(original);
        let cloned = owned.clone();
        assert!(cloned.will_wake(&target));
        assert!(!owned.will_wake(&target));

        let mut replaced = LocalWaker::from_waker(Waker::noop());
        replaced.clone_from(&owned);
        assert!(replaced.will_wake(&target));
        borrowed.wake_by_ref();
        cloned.wake();
        replaced.wake();

        let original = owned.into_waker();
        assert_eq!(original.data(), data);
        assert!(ptr::eq(original.vtable(), vtable));
        original.wake();
    }
    assert_eq!(counter.0.load(Ordering::Relaxed), 4);
    assert_eq!(Arc::strong_count(&counter), 1);
}

/// STATEMENT: Fallback wakers accept null data and preserve the destination when cloning panics.
///
/// HOW:
/// - Exercise a null-data waker whose clone panics, including same-identity replacement.
/// - Catch a failed replacement and verify the original destination still wakes its target.
#[test]
fn fallback_null_data_and_clone_unwind() {
    static PANIC: RawWakerVTable = RawWakerVTable::new(
        // Exercise unwinding without invoking panic hooks or backtraces.
        |_| resume_unwind(Box::new("clone")),
        |data| assert!(data.is_null()),
        |data| assert!(data.is_null()),
        |data| assert!(data.is_null()),
    );
    let waker = || {
        // SAFETY: The callbacks accept null data, never dereference it, and clone only panics.
        unsafe { Waker::from_raw(RawWaker::new(ptr::null(), &PANIC)) }
    };
    let source = LocalWaker::from(waker());
    let mut same = LocalWaker::from(waker());
    same.clone_from(&source); // Same identity must not call the panicking clone.
    same.wake_by_ref();
    same.wake();
    assert!(catch_unwind(|| LocalWaker::from_waker(&waker())).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| source.clone())).is_err());

    let counter = Arc::new(Counter::default());
    let target = Counter::waker(counter.clone());
    let mut destination = LocalWaker::from_waker(&target);
    assert!(catch_unwind(AssertUnwindSafe(|| destination.clone_from(&source))).is_err());
    assert!(destination.will_wake(&target));
    destination.wake();
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
}

/// STATEMENT: Panicking fallback wake and drop callbacks preserve reference ownership.
///
/// HOW:
/// - Invoke borrowed wake, consuming wake, and drop callbacks that each panic.
/// - Catch each panic and verify callback counts and surviving strong references.
#[test]
fn fallback_wake_and_drop_unwind() {
    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |data| {
            // SAFETY: `data` comes from `Arc::into_raw` and the source waker keeps a strong count
            // alive.
            unsafe { Arc::increment_strong_count(data.cast::<Counter>()) };
            RawWaker::new(data, &VTABLE)
        },
        |data| {
            // SAFETY: Consuming wake takes one raw strong count, which is dropped while unwinding.
            let counter = unsafe { Arc::from_raw(data.cast::<Counter>()) };
            counter.0.fetch_add(1, Ordering::Relaxed);
            resume_unwind(Box::new("wake"));
        },
        |data| {
            // SAFETY: The borrowed waker keeps `Counter` alive and its counter uses atomic
            // operations.
            unsafe { &*data.cast::<Counter>() }
                .0
                .fetch_add(1, Ordering::Relaxed);
            resume_unwind(Box::new("wake_by_ref"));
        },
        |data| {
            // SAFETY: The drop callback takes one raw strong count, which is dropped while
            // unwinding.
            let counter = unsafe { Arc::from_raw(data.cast::<Counter>()) };
            counter.0.fetch_add(1, Ordering::Relaxed);
            resume_unwind(Box::new("drop"));
        },
    );
    let counter = Arc::new(Counter::default());
    // SAFETY: The data owns an `Arc<Counter>` count; callbacks balance counts even when unwinding.
    let local = LocalWaker::from(unsafe {
        Waker::from_raw(RawWaker::new(
            Arc::into_raw(counter.clone()).cast(),
            &VTABLE,
        ))
    });
    assert!(catch_unwind(AssertUnwindSafe(|| local.wake_by_ref())).is_err());
    assert_eq!(Arc::strong_count(&counter), 2);
    assert!(catch_unwind(AssertUnwindSafe(|| local.clone().wake())).is_err());
    assert_eq!(Arc::strong_count(&counter), 2);
    assert!(catch_unwind(AssertUnwindSafe(|| drop(local))).is_err());
    assert_eq!(Arc::strong_count(&counter), 1);
    assert_eq!(counter.0.load(Ordering::Relaxed), 3);
}

/// STATEMENT: Local wakers specialize only native task wakers and preserve wrapper behavior.
///
/// HOW:
/// - Inspect a task's waker under the selected runtime and check native identity detection.
/// - Check diagnostics identify native and wrapped wakers without changing wake counts.
/// - Wake through a counting wrapper, then use retained wakers after task completion.
#[test]
fn selected_runtime_and_wrapped_waker() {
    struct Forward(Waker, Arc<Counter>);

    impl Wake for Forward {
        fn wake(self: Arc<Self>) {
            self.1.0.fetch_add(1, Ordering::Relaxed);
            self.0.wake_by_ref();
        }
    }

    crate::testing::TestRunner
        .block_on(Box::pin(async {
            let mut polled = false;
            let local = crate::spawn(poll_fn(move |cx| {
                let local = LocalWaker::from_waker(cx.waker());
                assert_eq!(
                    local.is_native(),
                    !cfg!(any(feature = "tokio", feature = "compio"))
                );
                assert!(local.will_wake(cx.waker()));
                assert_eq!(
                    format!("{local:?}"),
                    format!(
                        "LocalWaker {{ data: {:p}, native: {} }}",
                        local.data,
                        !cfg!(any(feature = "tokio", feature = "compio"))
                    )
                );
                if polled {
                    return Poll::Ready(local);
                }
                polled = true;
                let counter = Arc::new(Counter::default());
                let wrapped = Waker::from(Arc::new(Forward(cx.waker().clone(), counter.clone())));
                let handle = LocalWaker::from_waker(&wrapped);
                assert!(!handle.is_native());
                assert_eq!(
                    format!("{handle:?}"),
                    format!("LocalWaker {{ data: {:p}, native: false }}", handle.data)
                );
                assert_eq!(counter.0.load(Ordering::Relaxed), 0);
                handle.wake();
                assert_eq!(counter.0.load(Ordering::Relaxed), 1);
                local.wake();
                Poll::Pending
            }))
            .await
            .unwrap();
            local.clone().wake();
            local.into_waker().wake();
        }))
        .unwrap();
}
