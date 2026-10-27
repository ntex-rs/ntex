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
/// Native ntex task wakers acquired on their registered owner thread use local operations
/// without repeating the thread check or registry lookup. Other wakers retain their standard
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
        if let Some(ptr) = task::local_waker_ptr(waker) {
            unsafe { task::clone_local_waker(ptr) };
            Self::native(ptr)
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

    /// Consumes this handle to produce a thread-safe standard waker.
    ///
    /// Native conversion acquires standard-waker ownership before releasing the local reference.
    #[inline]
    pub fn into_waker(self) -> Waker {
        let this = ManuallyDrop::new(self);
        if this.is_native() {
            unsafe { task::into_waker(this.data) }
        } else {
            unsafe { Waker::from_raw(this.raw_waker()) }
        }
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
    /// Consumes a standard waker to acquire thread-confined wake ownership.
    #[inline]
    fn from(waker: Waker) -> Self {
        if let Some(ptr) = task::local_waker_ptr(&waker) {
            unsafe { task::clone_local_waker(ptr) };
            drop(waker);
            Self::native(ptr)
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
#[path = "../tests/unit/local_waker.rs"]
mod tests;
