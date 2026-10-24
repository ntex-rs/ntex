//! Task wake-up and scheduling utilities.
use std::{cell::Cell, fmt, task::Waker};

/// A single-threaded synchronization primitive for task wake-up.
///
/// Sometimes the task interested in a given event will change over time.
/// A `LocalWaker` can coordinate notifications with the consumer
/// potentially "updating" the underlying task to wake up. This is useful in
/// scenarios where a computation completes in another task and wants to
/// notify the consumer, but the consumer is in the process of being migrated to
/// a new logical task.
///
/// Consumers should call `register` before checking the result of a computation
/// and producers should call `wake` after producing the computation (this
/// differs from the usual `thread::park` pattern). It is also permitted for
/// `wake` to be called **before** `register`. This results in a no-op.
///
/// A single `LocalWaker` may be reused for any number of calls to `register` or
/// `wake`.
///
/// The slot and its owned local handle must remain on their acquiring thread.
/// Use [`take`](Self::take) to transfer a registered standard waker instead.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<ntex_util::task::LocalWaker>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<ntex_util::task::LocalWaker>();
/// ```
#[derive(Default)]
pub struct LocalWaker {
    waker: Cell<Option<ntex_rt::LocalWaker>>,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<LocalWaker>() == 16);

impl LocalWaker {
    /// Creates an empty `LocalWaker`.
    pub fn new() -> Self {
        LocalWaker::with(None)
    }

    /// Creates a `LocalWaker` with an optional initial waker.
    pub fn with(waker: Option<Waker>) -> Self {
        LocalWaker {
            waker: Cell::new(waker.map(ntex_rt::LocalWaker::from)),
        }
    }

    #[inline]
    /// Registers the waker to be notified on calls to `wake`.
    ///
    /// Returns `true` if a waker was already registered.
    ///
    /// If cloning the replacement panics, the previous registration is retained
    /// unless the clone callback itself changes the slot.
    pub fn register(&self, waker: &Waker) -> bool {
        let prev = self.waker.take();
        let registered = prev.is_some();
        let same = prev.as_ref().is_some_and(|prev| prev.will_wake(waker));
        self.waker.set(prev);
        if !same {
            // Clone and drop callbacks may reenter the slot. Keep the old
            // registration until cloning succeeds, then publish before dropping.
            let next = ntex_rt::LocalWaker::from_waker(waker);
            drop(self.waker.replace(Some(next)));
        }
        registered
    }

    #[inline]
    /// Calls `wake` on the last `Waker` passed to `register`.
    ///
    /// The waker is consumed, so a later `wake` does nothing until `register`
    /// is called again. If no waker is registered, this does nothing.
    pub fn wake(&self) {
        if let Some(waker) = self.take_local() {
            waker.wake();
        }
    }

    #[inline]
    /// Calls `wake` on the last `Waker` passed to `register`.
    ///
    /// Returns `false` if no waker is registered. Like [`wake`](Self::wake),
    /// this consumes the registered waker.
    pub fn wake_checked(&self) -> bool {
        if let Some(waker) = self.take_local() {
            waker.wake();
            true
        } else {
            false
        }
    }

    /// Returns the last `Waker` passed to `register`, so that the user can wake it.
    ///
    /// This empties the slot and converts the owned handle into a thread-safe
    /// standard waker without cloning. If no waker is registered, returns `None`.
    /// Use [`take_local`](Self::take_local) to retain native local operations.
    pub fn take(&self) -> Option<Waker> {
        self.take_local().map(ntex_rt::LocalWaker::into_waker)
    }

    /// Takes the registered wake handle, leaving the slot empty.
    ///
    /// The returned handle retains native local operations and cannot be sent
    /// to another thread. If no waker is registered, returns `None`.
    #[inline]
    pub fn take_local(&self) -> Option<ntex_rt::LocalWaker> {
        self.waker.take()
    }

    #[doc(hidden)]
    /// Returns `true` if a waker is registered.
    pub fn is_set(&self) -> bool {
        let waker = self.waker.take();
        let set = waker.is_some();
        self.waker.set(waker);
        set
    }
}

/// Cloning creates an empty `LocalWaker`; the registered waker is not copied.
impl Clone for LocalWaker {
    fn clone(&self) -> Self {
        LocalWaker::new()
    }
}

impl fmt::Debug for LocalWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LocalWaker")
    }
}

/// Yields execution back to the current runtime.
pub async fn yield_to() {
    use std::{future::Future, pin::Pin, task::Context, task::Poll};

    struct Yield {
        completed: bool,
    }

    impl Future for Yield {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.completed {
                return Poll::Ready(());
            }

            self.completed = true;
            cx.waker().wake_by_ref();

            Poll::Pending
        }
    }

    Yield { completed: false }.await;
}

#[cfg(test)]
mod test {
    use super::*;

    #[ntex::test]
    async fn yield_test() {
        yield_to().await;
    }

    #[ntex::test]
    async fn local_slot_wakes_current_task() {
        crate::spawn(async {
            let slot = LocalWaker::new();
            let mut polls = 0;
            std::future::poll_fn(|cx| {
                polls += 1;
                if polls == 4 {
                    return std::task::Poll::Ready(());
                }
                assert!(!slot.register(cx.waker()));
                match polls {
                    1 => slot.wake(),
                    2 => slot.take_local().unwrap().wake(),
                    _ => slot.take().unwrap().wake(),
                }
                std::task::Poll::Pending
            })
            .await;
            assert!(!slot.is_set());
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{RawWaker, RawWakerVTable, Wake};
    use std::thread::LocalKey;

    use super::*;

    static CLONES: AtomicUsize = AtomicUsize::new(0);

    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |p| {
            CLONES.fetch_add(1, Ordering::Relaxed);
            RawWaker::new(p, &VTABLE)
        },
        |_| {},
        |_| {},
        |_| {},
    );

    type Hook = Cell<Option<Box<dyn FnOnce()>>>;

    // Waker callbacks may run on any thread; local test closures stay in TLS.
    thread_local! {
        static ON_CLONE: Hook = Cell::new(None);
        static ON_DROP: Hook = Cell::new(None);
        static ON_WAKE: Hook = Cell::new(None);
    }

    fn run_hook(key: &'static LocalKey<Hook>) {
        if let Some(hook) = key.with(Cell::take) {
            hook();
        }
    }

    fn hook_waker() -> Waker {
        static HOOK_VTABLE: RawWakerVTable = RawWakerVTable::new(
            |p| {
                run_hook(&ON_CLONE);
                RawWaker::new(p, &HOOK_VTABLE)
            },
            |_| run_hook(&ON_WAKE),
            |_| run_hook(&ON_WAKE),
            |_| run_hook(&ON_DROP),
        );
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &HOOK_VTABLE)) }
    }

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn counted_waker() -> (Arc<WakeCounter>, Waker) {
        let count = Arc::new(WakeCounter::default());
        let waker = Waker::from(count.clone());
        (count, waker)
    }

    #[test]
    fn test_register_same_waker() {
        static A: u8 = 0;
        static B: u8 = 0;
        let a = unsafe { Waker::from_raw(RawWaker::new((&raw const A).cast(), &VTABLE)) };
        let b = unsafe { Waker::from_raw(RawWaker::new((&raw const B).cast(), &VTABLE)) };

        let w = LocalWaker::new();
        assert!(!w.register(&a));
        assert!(w.register(&a));
        assert!(w.register(&a));
        assert_eq!(CLONES.load(Ordering::Relaxed), 1);

        assert!(w.register(&b));
        assert_eq!(CLONES.load(Ordering::Relaxed), 2);
        assert!(w.take().unwrap().will_wake(&b));
    }

    #[test]
    fn local_waker_clone_is_empty() {
        let waker = LocalWaker::new();
        waker.register(std::task::Waker::noop());
        assert!(waker.is_set());
        let cloned = waker.clone();
        assert!(!cloned.is_set());
        assert!(!cloned.wake_checked());
        assert!(waker.is_set());
        assert_eq!(format!("{waker:?}"), "LocalWaker");
    }

    #[test]
    fn owning_conversions_and_wakes_do_not_clone() {
        ON_CLONE.set(Some(Box::new(|| panic!("unexpected clone"))));
        let calls = Rc::new(Cell::new(0));
        for path in 0..4 {
            let calls = calls.clone();
            ON_WAKE.set(Some(Box::new(move || calls.set(calls.get() + 1))));
            let slot = LocalWaker::with(Some(hook_waker()));
            match path {
                0 => slot.take().unwrap().wake(),
                1 => slot.take_local().unwrap().wake(),
                2 => slot.wake(),
                _ => assert!(slot.wake_checked()),
            }
            assert!(slot.take().is_none());
            assert!(slot.take_local().is_none());
            assert!(!slot.wake_checked());
        }
        assert_eq!(calls.get(), 4);
        assert!(ON_CLONE.with(Cell::take).is_some());
    }

    #[test]
    fn clone_panic_preserves_registration() {
        let (calls, original) = counted_waker();
        let slot = LocalWaker::with(Some(original));
        let replacement = hook_waker();
        ON_CLONE.set(Some(Box::new(|| panic!("clone"))));
        assert!(catch_unwind(AssertUnwindSafe(|| slot.register(&replacement))).is_err());
        assert!(slot.wake_checked());
        assert_eq!(calls.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn clone_reentry_preserves_nested_changes_on_unwind() {
        for panic in [false, true] {
            let (previous_calls, previous) = counted_waker();
            let (nested_calls, nested) = counted_waker();
            let slot = Rc::new(LocalWaker::with(Some(previous)));
            let callback_slot = slot.clone();
            ON_CLONE.set(Some(Box::new(move || {
                assert!(callback_slot.wake_checked());
                assert!(!callback_slot.register(&nested));
                assert!(!panic, "clone");
            })));
            let replacement = hook_waker();
            let result = catch_unwind(AssertUnwindSafe(|| slot.register(&replacement)));
            if panic {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap());
            }
            assert_eq!(previous_calls.0.load(Ordering::Relaxed), 1);
            assert!(slot.wake_checked());
            assert_eq!(nested_calls.0.load(Ordering::Relaxed), usize::from(panic));
        }
    }

    #[test]
    fn replaced_drop_reentry_and_unwind() {
        for panic in [false, true] {
            let (replacement_calls, replacement) = counted_waker();
            let (nested_calls, nested) = counted_waker();
            let slot = Rc::new(LocalWaker::with(Some(hook_waker())));
            let callback_slot = slot.clone();
            ON_DROP.set(Some(Box::new(move || {
                assert!(callback_slot.wake_checked());
                assert!(!callback_slot.register(&nested));
                assert!(!panic, "drop");
            })));
            let result = catch_unwind(AssertUnwindSafe(|| slot.register(&replacement)));
            if panic {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap());
            }
            assert_eq!(replacement_calls.0.load(Ordering::Relaxed), 1);
            assert!(slot.wake_checked());
            assert_eq!(nested_calls.0.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn wake_reentry_and_unwind() {
        for panic in [false, true] {
            for checked in [false, true] {
                let (calls, nested) = counted_waker();
                let slot = Rc::new(LocalWaker::with(Some(hook_waker())));
                let callback_slot = slot.clone();
                ON_WAKE.set(Some(Box::new(move || {
                    assert!(!callback_slot.is_set());
                    assert!(!callback_slot.register(&nested));
                    assert!(!panic, "wake");
                })));
                let result = catch_unwind(AssertUnwindSafe(|| {
                    if checked {
                        assert!(slot.wake_checked());
                    } else {
                        slot.wake();
                    }
                }));
                assert_eq!(result.is_err(), panic);
                assert!(slot.wake_checked());
                assert_eq!(calls.0.load(Ordering::Relaxed), 1);
            }
        }
    }

    #[test]
    fn native_slot_delivery_and_shutdown() {
        #[derive(Debug)]
        struct NoopNotify;

        impl ntex_rt::Notify for NoopNotify {
            fn notify(&self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let rt = ntex_rt::Runtime::new(Box::new(NoopNotify));
        let slot = Rc::new(LocalWaker::new());
        let polls = Rc::new(Cell::new(0));
        let (task_slot, task_polls) = (slot.clone(), polls.clone());
        let task = rt.spawn(std::future::poll_fn(move |cx| {
            task_polls.set(task_polls.get() + 1);
            task_slot.register(cx.waker());
            std::task::Poll::<()>::Pending
        }));
        rt.poll();
        slot.take_local().unwrap().wake();
        rt.poll();
        let standard = slot.take().unwrap();
        std::thread::spawn(move || standard.wake()).join().unwrap();
        rt.poll();
        assert_eq!(polls.get(), 3);
        assert_eq!(Rc::strong_count(&slot), 2);
        drop(rt);
        assert!(task.is_finished());
        assert_eq!(Rc::strong_count(&slot), 1);
        assert!(slot.wake_checked());
        assert!(!slot.wake_checked());
    }
}
