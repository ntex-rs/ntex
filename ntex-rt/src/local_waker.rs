use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::task::{RawWaker, RawWakerVTable, Waker};
use std::{fmt, rc::Rc};

use crate::rt::task;

// Address 1 cannot be an aligned vtable pointer and leaves the null niche free.
const NATIVE: NonNull<()> = NonNull::dangling();
const _: () = assert!(align_of::<RawWakerVTable>() > 1);

/// An owned wake handle that can only be used on the thread that acquired it.
///
/// Native ntex task wakers acquired on their owner thread use local operations
/// without repeating the thread check. Other wakers retain their standard
/// behavior, including wakers from Tokio, Compio, and custom wrappers.
///
/// Cloning this handle clones wake ownership; it does not create an empty
/// notification slot. Waking consumes the handle, while [`wake_by_ref`](Self::wake_by_ref)
/// retains it. A handle remains valid after its task completes or its runtime
/// is dropped. Use [`into_waker`](Self::into_waker) to transfer it to another
/// thread as a standard, thread-safe [`Waker`].
///
/// On 64-bit targets both this type and `Option<LocalWaker>` occupy 16 bytes.
/// There is no additional wrapper allocation; a foreign waker's own clone
/// operation may allocate.
///
/// ```
/// use ntex_rt::LocalWaker;
/// use std::task::Waker;
///
/// let local = LocalWaker::from_waker(Waker::noop());
/// local.wake_by_ref();
/// let waker = local.into_waker();
/// std::thread::spawn(move || waker.wake()).join().unwrap();
/// ```
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<ntex_rt::LocalWaker>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<ntex_rt::LocalWaker>();
/// ```
#[repr(C)]
pub struct LocalWaker {
    data: *const (),
    // NATIVE on the owner thread, otherwise the original standard vtable.
    metadata: NonNull<()>,
    _local: PhantomData<Rc<()>>,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<LocalWaker>() == 16);
    assert!(size_of::<Option<LocalWaker>>() == 16);
};

impl LocalWaker {
    /// Clones a standard waker into a thread-confined wake handle.
    #[inline]
    pub fn from_waker(waker: &Waker) -> Self {
        if task::is_local_waker(waker) {
            // Recognition checked both the vtable and the current thread.
            unsafe { task::clone_local_waker(waker.data()) };
            Self::native(waker.data())
        } else {
            Self::fallback(waker.clone())
        }
    }

    /// Wakes the associated task, consuming this handle.
    #[inline]
    pub fn wake(self) {
        if self.is_native() {
            let this = ManuallyDrop::new(self);
            unsafe { task::wake_local_waker(this.data, true) };
        } else {
            self.into_waker().wake();
        }
    }

    /// Wakes the associated task without consuming this handle.
    #[inline]
    pub fn wake_by_ref(&self) {
        if self.is_native() {
            unsafe { task::wake_local_waker(self.data, false) };
        } else {
            self.borrowed_waker().wake_by_ref();
        }
    }

    /// Returns whether this handle and `waker` wake the same task.
    ///
    /// Like [`Waker::will_wake`], `true` guarantees the same task, but `false`
    /// may be returned for different representations of the same task.
    #[inline]
    pub fn will_wake(&self, waker: &Waker) -> bool {
        self.borrowed_waker().will_wake(waker)
    }

    /// Converts this handle into a thread-safe standard waker without cloning.
    #[inline]
    pub fn into_waker(self) -> Waker {
        let this = ManuallyDrop::new(self);
        // Transfer the owned reference using the thread-safe native vtable
        // or the original foreign vtable, never local-only callbacks.
        unsafe { Waker::from_raw(this.raw_waker()) }
    }

    fn native(data: *const ()) -> Self {
        Self {
            data,
            metadata: NATIVE,
            _local: PhantomData,
        }
    }

    fn fallback(waker: Waker) -> Self {
        let waker = ManuallyDrop::new(waker);
        Self {
            data: waker.data(),
            metadata: NonNull::from(waker.vtable()).cast(),
            _local: PhantomData,
        }
    }

    fn is_native(&self) -> bool {
        self.metadata == NATIVE
    }

    fn raw_waker(&self) -> RawWaker {
        if self.is_native() {
            task::raw_waker(self.data)
        } else {
            // Fallback metadata came from Waker::vtable(), a static reference.
            RawWaker::new(self.data, unsafe {
                self.metadata.cast::<RawWakerVTable>().as_ref()
            })
        }
    }

    fn borrowed_waker(&self) -> ManuallyDrop<Waker> {
        // The temporary borrows this handle's reference, it must not release it.
        ManuallyDrop::new(unsafe { Waker::from_raw(self.raw_waker()) })
    }
}

impl From<Waker> for LocalWaker {
    /// Takes ownership of a standard waker without cloning it.
    #[inline]
    fn from(waker: Waker) -> Self {
        if task::is_local_waker(&waker) {
            Self::native(ManuallyDrop::new(waker).data())
        } else {
            Self::fallback(waker)
        }
    }
}

impl Clone for LocalWaker {
    #[inline]
    fn clone(&self) -> Self {
        if self.is_native() {
            unsafe { task::clone_local_waker(self.data) };
            Self::native(self.data)
        } else {
            Self::fallback((*self.borrowed_waker()).clone())
        }
    }

    #[inline]
    fn clone_from(&mut self, source: &Self) {
        if self.data != source.data || self.metadata != source.metadata {
            *self = source.clone();
        }
    }
}

impl Drop for LocalWaker {
    #[inline]
    fn drop(&mut self) {
        if self.is_native() {
            unsafe { task::drop_local_waker(self.data) };
        } else {
            drop(unsafe { Waker::from_raw(self.raw_waker()) });
        }
    }
}

impl fmt::Debug for LocalWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalWaker")
            .field("data", &self.data)
            .field("native", &self.is_native())
            .finish()
    }
}

#[cfg(all(test, not(ntex_loom)))]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
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
                unsafe { Arc::increment_strong_count(data.cast::<Counter>()) };
                RawWaker::new(data, &VTABLE)
            }
            unsafe fn wake(data: *const ()) {
                let counter = unsafe { Arc::from_raw(data.cast::<Counter>()) };
                counter.0.fetch_add(1, Ordering::Relaxed);
            }
            unsafe fn wake_by_ref(data: *const ()) {
                unsafe { &*data.cast::<Counter>() }
                    .0
                    .fetch_add(1, Ordering::Relaxed);
            }
            unsafe fn drop(data: *const ()) {
                std::mem::drop(unsafe { Arc::from_raw(data.cast::<Counter>()) });
            }
            static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
            unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(counter).cast(), &VTABLE)) }
        }
    }

    // Cloning changes both raw parts to those of the inner waker.
    fn changing_waker(waker: Waker) -> Waker {
        unsafe fn clone(data: *const ()) -> RawWaker {
            let waker = ManuallyDrop::new(unsafe { &*data.cast::<Waker>() }.clone());
            RawWaker::new(waker.data(), waker.vtable())
        }
        unsafe fn wake(data: *const ()) {
            unsafe { Box::from_raw(data.cast_mut().cast::<Waker>()) }.wake();
        }
        unsafe fn wake_by_ref(data: *const ()) {
            unsafe { &*data.cast::<Waker>() }.wake_by_ref();
        }
        unsafe fn drop(data: *const ()) {
            std::mem::drop(unsafe { Box::from_raw(data.cast_mut().cast::<Waker>()) });
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        unsafe {
            Waker::from_raw(RawWaker::new(
                Box::into_raw(Box::new(waker)).cast(),
                &VTABLE,
            ))
        }
    }

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

    #[test]
    fn fallback_null_data_and_clone_unwind() {
        static PANIC: RawWakerVTable = RawWakerVTable::new(
            |_| panic!("clone"),
            |data| assert!(data.is_null()),
            |data| assert!(data.is_null()),
            |data| assert!(data.is_null()),
        );
        let waker = || unsafe { Waker::from_raw(RawWaker::new(ptr::null(), &PANIC)) };
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

    #[test]
    fn fallback_wake_and_drop_unwind() {
        static VTABLE: RawWakerVTable = RawWakerVTable::new(
            |data| {
                unsafe { Arc::increment_strong_count(data.cast::<Counter>()) };
                RawWaker::new(data, &VTABLE)
            },
            |data| {
                let counter = unsafe { Arc::from_raw(data.cast::<Counter>()) };
                counter.0.fetch_add(1, Ordering::Relaxed);
                panic!("wake");
            },
            |data| {
                unsafe { &*data.cast::<Counter>() }
                    .0
                    .fetch_add(1, Ordering::Relaxed);
                panic!("wake_by_ref");
            },
            |data| {
                let counter = unsafe { Arc::from_raw(data.cast::<Counter>()) };
                counter.0.fetch_add(1, Ordering::Relaxed);
                panic!("drop");
            },
        );
        let counter = Arc::new(Counter::default());
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
                    if polled {
                        return Poll::Ready(local);
                    }
                    polled = true;
                    let counter = Arc::new(Counter::default());
                    let wrapped =
                        Waker::from(Arc::new(Forward(cx.waker().clone(), counter.clone())));
                    let handle = LocalWaker::from_waker(&wrapped);
                    assert!(!handle.is_native());
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
}
