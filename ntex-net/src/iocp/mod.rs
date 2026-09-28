use std::{fmt, mem, ops::Deref, ops::DerefMut, ptr};

use socket2::{SockAddr, Socket};
use windows_sys::Win32::System::IO::OVERLAPPED;

mod connect;
mod io;
mod ops;
mod reactor;
mod stream;

pub use self::reactor::{Handler, Reactor, ReactorApi};

/// Tcp stream wrapper for neon `TcpStream`
struct TcpStream(Socket, SockAddr, stream::StreamOps);

/// Tcp stream wrapper for neon `UnixStream`
struct UnixStream(Socket, SockAddr, stream::StreamOps);

/// The overlapped struct for IOCP ops.
#[repr(C)]
pub struct Overlapped {
    /// The base [`OVERLAPPED`].
    pub(crate) base: OVERLAPPED,
    /// User data
    pub(crate) hnd: u32,
    pub(crate) udata: u32,
    /// Pointer to `base`, handed to the kernel. Set by [`OpBox::new`] from
    /// the pointer that owns the op, so it may write to the whole op.
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

/// An op that embeds an [`Overlapped`].
///
/// # Safety
///
/// `overlapped` must return a pointer to the embedded `Overlapped`, derived
/// from `this` without creating a reference.
pub(crate) unsafe trait OverlappedOp {
    unsafe fn overlapped(this: *mut Self) -> *mut Overlapped;
}

/// Owns a heap allocated IOCP op.
///
/// The kernel writes to an op while it is in flight, and the completion
/// accesses the whole op through the pointer returned by the kernel. That
/// pointer is derived from the one owning the allocation, not from a
/// reference, so it stays valid while the op is accessed through the box.
/// A `Box` cannot be used, moving it asserts unique access and would
/// invalidate the pointer held by the kernel.
pub(crate) struct OpBox<T: OverlappedOp>(ptr::NonNull<T>);

impl<T: OverlappedOp> OpBox<T> {
    pub(crate) fn new(op: T) -> Self {
        let ptr = Box::into_raw(Box::new(op));
        // SAFETY: `ptr` is a fresh allocation, `T::overlapped` points into it
        unsafe {
            let ov = T::overlapped(ptr);
            (*ov).this = &raw mut (*ov).base;
            OpBox(ptr::NonNull::new_unchecked(ptr))
        }
    }

    /// Returns the pointer owning the op.
    pub(crate) fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    /// Releases the allocation without freeing it.
    pub(crate) fn into_raw(self) -> *mut T {
        mem::ManuallyDrop::new(self).0.as_ptr()
    }

    /// Returns the op, the kernel must not reference it anymore.
    pub(crate) fn into_inner(self) -> T {
        // SAFETY: the allocation comes from `Box::into_raw` and is released once
        unsafe { *Box::from_raw(self.into_raw()) }
    }
}

impl<T: OverlappedOp> Deref for OpBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the allocation is valid until `OpBox` is dropped
        unsafe { self.0.as_ref() }
    }
}

impl<T: OverlappedOp> DerefMut for OpBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the allocation is valid until `OpBox` is dropped
        unsafe { self.0.as_mut() }
    }
}

impl<T: OverlappedOp> Drop for OpBox<T> {
    fn drop(&mut self) {
        // SAFETY: the allocation comes from `Box::into_raw` and is released once
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

impl<T: OverlappedOp + fmt::Debug> fmt::Debug for OpBox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct TestOp {
        overlapped: Overlapped,
        value: usize,
    }

    unsafe impl OverlappedOp for TestOp {
        unsafe fn overlapped(this: *mut Self) -> *mut Overlapped {
            unsafe { &raw mut (*this).overlapped }
        }
    }

    #[test]
    fn miri_opbox_kernel_pointer() {
        let mut op = OpBox::new(TestOp {
            overlapped: Overlapped::new(1, 2),
            value: 0,
        });
        let optr = op.overlapped.as_overlapped();
        assert_eq!(optr, op.as_ptr().cast());

        // the op is used through the box while the "kernel" holds `optr`
        op.value = 1;
        assert_eq!(op.overlapped.user_data(), 2);

        // the kernel writes the status, the completion takes the whole op
        unsafe {
            (*optr).Internal = 7;
            let done = &mut *optr.cast::<TestOp>();
            done.value += 1;
        }
        assert_eq!(op.overlapped.base.Internal, 7);
        assert_eq!(op.value, 2);

        let op = op.into_inner();
        assert_eq!(op.value, 2);

        // a leaked op stays valid for the kernel
        let raw = OpBox::new(TestOp {
            overlapped: Overlapped::new(1, 3),
            value: 5,
        })
        .into_raw();
        unsafe {
            (*(*raw).overlapped.as_overlapped()).Internal = 1;
            drop(Box::from_raw(raw));
        }
    }
}
