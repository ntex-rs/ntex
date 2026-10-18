use std::{fmt, mem, net, ops::Deref, ops::DerefMut, ptr};

use socket2::Socket;
use windows_sys::Win32::System::IO::OVERLAPPED;

mod connect;
mod io;
mod ops;
mod reactor;
mod stream;

#[cfg(feature = "compio")]
pub(crate) use self::reactor::map_socket_error;
pub use self::reactor::{Handler, Reactor, ReactorApi};

/// Tcp stream wrapper for neon `TcpStream`
struct TcpStream(Socket, Option<net::SocketAddr>, stream::StreamOps);

/// Tcp stream wrapper for neon `UnixStream`
struct UnixStream(Socket, stream::StreamOps);

/// The overlapped struct for IOCP ops.
#[repr(C)]
pub struct Overlapped {
    /// The base [`OVERLAPPED`].
    pub(crate) base: OVERLAPPED,
    /// User data
    pub(crate) hnd: u32,
    pub(crate) udata: u32,
    /// Pointer to `base`, handed to the kernel. Set by [`OpBox::new`] from
    /// the allocation pointer, preserving provenance for completion handling.
    this: *mut OVERLAPPED,
}

impl Overlapped {
    pub(crate) fn new(hnd: u32, udata: u32) -> Self {
        Self {
            hnd,
            udata,
            base: unsafe { std::mem::zeroed() },
            this: ptr::null_mut(),
        }
    }

    /// Returns the pointer passed to the kernel for ops of an [`OpBox`].
    pub fn as_overlapped(&self) -> *mut OVERLAPPED {
        debug_assert!(!self.this.is_null(), "Overlapped is not owned by an OpBox");
        self.this
    }

    pub fn user_data(&self) -> u32 {
        self.udata
    }
}

impl fmt::Debug for Overlapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Overlapped")
            .field("base", &"OVERLAPPED")
            .field("hnd", &self.hnd)
            .field("udata", &self.udata)
            .finish()
    }
}

#[repr(C)]
pub(crate) struct Op<T> {
    overlapped: Overlapped,
    pub(crate) state: T,
}

/// Owns a heap allocated IOCP op.
///
/// The kernel owns the header while an op is in flight. All references
/// project only the disjoint application state, never the entire allocation
/// or header. The kernel pointer retains the allocation's provenance so a
/// completion can recover the state, including when it resubmits the op.
/// A `Box` cannot be kept as the owner: moving it asserts unique access to
/// the whole allocation, invalidating the pointer held by the kernel.
pub(crate) struct OpBox<T>(ptr::NonNull<Op<T>>);

impl<T> OpBox<T> {
    pub(crate) fn new(overlapped: Overlapped, state: T) -> Self {
        let ptr = Box::into_raw(Box::new(Op { overlapped, state }));
        // SAFETY: `ptr` is a fresh allocation, its header is at offset zero
        unsafe {
            (*ptr).overlapped.this = ptr.cast();
            OpBox(ptr::NonNull::new_unchecked(ptr))
        }
    }

    /// Returns a pointer to the application state, excluding the kernel header.
    pub(crate) fn as_ptr(&self) -> *mut T {
        // SAFETY: the allocation is valid until `OpBox` is dropped
        unsafe { &raw mut (*self.0.as_ptr()).state }
    }

    /// Returns the kernel header without borrowing it.
    pub(crate) fn as_overlapped(&self) -> *mut OVERLAPPED {
        self.0.as_ptr().cast()
    }

    /// Recovers application state from a completed operation's kernel pointer.
    ///
    /// # Safety
    ///
    /// `optr` must come from a live `OpBox<T>` with the matching `T`.
    /// Access through the result must not alias other references to the state.
    pub(crate) unsafe fn data_from_overlapped(optr: *mut Overlapped) -> *mut T {
        unsafe { &raw mut (*optr.cast::<Op<T>>()).state }
    }

    /// Releases the allocation without freeing it.
    pub(crate) fn into_raw(self) -> *mut Op<T> {
        mem::ManuallyDrop::new(self).0.as_ptr()
    }

    /// Returns the op, the kernel must not reference it anymore.
    pub(crate) fn into_inner(self) -> T {
        // SAFETY: the allocation comes from `Box::into_raw` and is released once
        unsafe { Box::from_raw(self.into_raw()).state }
    }
}

impl<T> Deref for OpBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the state is valid and disjoint from the kernel-owned header
        unsafe { &*self.as_ptr() }
    }
}

impl<T> DerefMut for OpBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the state is exclusively borrowed, not the kernel-owned header
        unsafe { &mut *self.as_ptr() }
    }
}

impl<T> Drop for OpBox<T> {
    fn drop(&mut self) {
        // SAFETY: the allocation comes from `Box::into_raw` and is released once
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

impl<T: fmt::Debug> fmt::Debug for OpBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestOp {
        value: usize,
    }

    // Model an inline kernel write while the payload's mutable borrow is
    // protected, including a payload access after the kernel write.
    #[inline(never)]
    fn issue(op: &mut TestOp, optr: *mut OVERLAPPED) {
        unsafe { (*optr).Internal = 7 };
        op.value += 1;
    }

    #[test]
    fn miri_opbox_inline_completion() {
        let mut op = OpBox::new(Overlapped::new(1, 2), TestOp { value: 0 });
        let optr = op.as_overlapped();
        issue(&mut op, optr);
        assert_eq!(op.value, 1);

        // Completion recovers only the payload and can immediately resubmit.
        unsafe {
            let done = &mut *OpBox::<TestOp>::data_from_overlapped(optr.cast());
            issue(done, optr);
            assert_eq!(done.value, 2);
            assert_eq!((*optr).Internal, 7);
        }
    }

    #[test]
    fn miri_opbox_kernel_pointer() {
        #[inline(never)]
        fn inspect(op: &TestOp, optr: *mut OVERLAPPED) {
            assert_eq!(op.value, 1);
            unsafe { (*optr).Internal = 7 };
            assert_eq!(format!("{op:?}"), "TestOp { value: 1 }");
        }

        let op = OpBox::new(Overlapped::new(1, 2), TestOp { value: 0 });
        let optr = op.as_overlapped();
        // Move the owner while the kernel holds its pointer.
        let mut ops = vec![op];
        let op = &mut ops[0];
        op.value = 1;
        inspect(op, optr);
        issue(op, optr);

        unsafe {
            let done = &mut *OpBox::<TestOp>::data_from_overlapped(optr.cast());
            done.value += 1;
            assert_eq!((*optr).Internal, 7);
        }
        assert_eq!(op.value, 3);
        assert_eq!(format!("{op:?}"), "TestOp { value: 3 }");
        let op = ops.pop().unwrap().into_inner();
        assert_eq!(op.value, 3);
    }

    #[test]
    fn miri_opbox_released_allocation() {
        let op = OpBox::new(Overlapped::new(1, 3), TestOp { value: 5 });
        let optr = op.as_overlapped();
        let raw = op.into_raw();
        // Cleanup can move out the payload, leaving a live header for the kernel.
        unsafe {
            let state = ptr::read(&raw const (*raw).state);
            (*optr).Internal = 1;
            assert_eq!(state.value, 5);
            assert_eq!((*optr.cast::<Overlapped>()).udata, 3);
            // Restore the payload before freeing this test's allocation.
            ptr::write(&raw mut (*raw).state, state);
            drop(Box::from_raw(raw));
        }
    }
}
