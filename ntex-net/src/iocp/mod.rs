use std::{fmt, marker::PhantomData, mem, net, ops::Deref, ops::DerefMut, ptr};

use socket2::Socket;
use windows_sys::Win32::System::IO::OVERLAPPED;

mod connect;
mod io;
mod ops;
mod reactor;
mod stream;

pub use self::reactor::Reactor;
#[cfg(feature = "compio")]
pub(crate) use self::reactor::map_socket_error;
pub(crate) use self::reactor::{Handler, ReactorApi};

/// Tcp stream wrapper for neon `TcpStream`
struct TcpStream(Socket, Option<net::SocketAddr>, stream::StreamOps);

/// Tcp stream wrapper for neon `UnixStream`
struct UnixStream(Socket, stream::StreamOps);

/// The overlapped struct for IOCP ops.
#[repr(C)]
pub(crate) struct Overlapped {
    /// The base [`OVERLAPPED`].
    pub(crate) base: OVERLAPPED,
    /// User data
    pub(crate) hnd: u32,
    pub(crate) udata: u32,
}

impl Overlapped {
    pub(crate) fn new(hnd: u32, udata: u32) -> Self {
        Self {
            hnd,
            udata,
            base: unsafe { std::mem::zeroed() },
        }
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

impl<T> Op<T> {
    /// Recovers application state from a completed operation's kernel pointer.
    ///
    /// # Safety
    ///
    /// `optr` must point to a live `Op<T>` with the matching `T`, either
    /// separately allocated or part of an `OpPairBox`.
    /// Access through the result must not alias other references to the state.
    pub(crate) unsafe fn data_from_overlapped(optr: *mut Overlapped) -> *mut T {
        unsafe { &raw mut (*optr.cast::<Self>()).state }
    }
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
        // SAFETY: `ptr` is a fresh, non-null allocation
        unsafe { OpBox(ptr::NonNull::new_unchecked(ptr)) }
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

/// Stream operations in one allocation, the write op comes first so that its
/// header is at the start in both layouts. `R` is either the read op with its
/// header, or the bare read payload.
#[repr(C)]
struct OpPair<W, R> {
    write: Op<W>,
    read: R,
}

/// Tags an `OpPairBox` pointer whose read payload has no kernel header.
const NO_READ_HEADER: usize = 1;

/// Owns both stream operations in one allocation.
///
/// Like `OpBox`, only payloads are borrowed, never their kernel-owned headers.
/// The payloads are also disjoint from each other, so a read can issue a write
/// reentrantly. Both operations must be finished before dropping the owner.
///
/// A read that never waits with an overlapped recv, as with socket state
/// notifications, is allocated without a header. The layout is fixed at
/// construction and tagged in the pointer, so the owner stays one word.
pub(crate) struct OpPairBox<R, W>(ptr::NonNull<Op<W>>, PhantomData<R>);

impl<R, W> OpPairBox<R, W> {
    /// Allocates both operations, the read op without a kernel header if
    /// `read_overlapped` is `None`.
    pub(crate) fn new(
        read_overlapped: Option<Overlapped>,
        read: R,
        write_overlapped: Overlapped,
        write: W,
    ) -> Self {
        const { assert!(mem::align_of::<Op<W>>() > NO_READ_HEADER) };

        let write = Op {
            overlapped: write_overlapped,
            state: write,
        };
        let ptr = if let Some(overlapped) = read_overlapped {
            let read = Op {
                overlapped,
                state: read,
            };
            Box::into_raw(Box::new(OpPair { write, read })).cast::<Op<W>>()
        } else {
            Box::into_raw(Box::new(OpPair { write, read }))
                .cast::<Op<W>>()
                .map_addr(|addr| addr | NO_READ_HEADER)
        };
        // SAFETY: `ptr` is a fresh, non-null allocation, tagging keeps it non-null
        unsafe { Self(ptr::NonNull::new_unchecked(ptr), PhantomData) }
    }

    /// Whether the read op has a kernel header.
    pub(crate) fn has_read_header(&self) -> bool {
        self.0.as_ptr().addr() & NO_READ_HEADER == 0
    }

    fn base(&self) -> *mut Op<W> {
        self.0.as_ptr().map_addr(|addr| addr & !NO_READ_HEADER)
    }

    fn full(&self) -> *mut OpPair<W, Op<R>> {
        debug_assert!(self.has_read_header());
        self.base().cast()
    }

    fn bare(&self) -> *mut OpPair<W, R> {
        debug_assert!(!self.has_read_header());
        self.base().cast()
    }

    pub(crate) fn read_ptr(&self) -> *mut R {
        // SAFETY: the allocation is valid until the owner is dropped, and has
        // the layout of its tag
        unsafe {
            if self.has_read_header() {
                &raw mut (*self.full()).read.state
            } else {
                &raw mut (*self.bare()).read
            }
        }
    }

    /// Returns the read op's kernel header, null if it has none.
    pub(crate) fn read_overlapped(&self) -> *mut OVERLAPPED {
        if self.has_read_header() {
            // SAFETY: project from the root pointer without borrowing either op
            unsafe { (&raw mut (*self.full()).read).cast() }
        } else {
            ptr::null_mut()
        }
    }

    pub(crate) fn read(&self) -> &R {
        // SAFETY: only the read payload is borrowed, not either header or write
        unsafe { &*self.read_ptr() }
    }

    pub(crate) fn read_mut(&mut self) -> &mut R {
        // SAFETY: the read payload is exclusively borrowed, not either header
        unsafe { &mut *self.read_ptr() }
    }

    pub(crate) fn write_ptr(&self) -> *mut W {
        // SAFETY: the allocation is valid until the owner is dropped
        unsafe { &raw mut (*self.base()).state }
    }

    pub(crate) fn write_overlapped(&self) -> *mut OVERLAPPED {
        // the header is at the start of the allocation in both layouts
        self.base().cast()
    }

    pub(crate) fn write(&self) -> &W {
        // SAFETY: only the write payload is borrowed, not either header or read
        unsafe { &*self.write_ptr() }
    }

    pub(crate) fn write_mut(&mut self) -> &mut W {
        // SAFETY: the write payload is exclusively borrowed, not either header
        unsafe { &mut *self.write_ptr() }
    }
}

impl<R, W> Drop for OpPairBox<R, W> {
    fn drop(&mut self) {
        // SAFETY: the allocation comes from `Box::into_raw` with the layout of
        // its tag, and is released once
        unsafe {
            if self.has_read_header() {
                drop(Box::from_raw(self.full()));
            } else {
                drop(Box::from_raw(self.bare()));
            }
        }
    }
}

impl<R: fmt::Debug, W: fmt::Debug> fmt::Debug for OpPairBox<R, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpPairBox")
            .field("read", self.read())
            .field("write", self.write())
            .finish()
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
            let done = &mut *Op::<TestOp>::data_from_overlapped(optr.cast());
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
            let done = &mut *Op::<TestOp>::data_from_overlapped(optr.cast());
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

    #[test]
    fn miri_op_pair_reentrant_access() {
        #[inline(never)]
        fn read_and_write(
            read: &mut TestOp,
            read_header: *mut OVERLAPPED,
            owners: &mut Vec<OpPairBox<TestOp, TestOp>>,
        ) {
            issue(read, read_header);
            owners.reserve(1024);
            let owner = &mut owners[0];
            let write_header = owner.write_overlapped();
            issue(owner.write_mut(), write_header);
            // The read borrow survives moving the owner and a nested write.
            read.value += 1;
        }

        let pair = OpPairBox::new(
            Some(Overlapped::new(1, 1)),
            TestOp { value: 0 },
            Overlapped::new(1, 2),
            TestOp { value: 10 },
        );
        let read_header = pair.read_overlapped();
        let write_header = pair.write_overlapped();
        let read = pair.read_ptr();
        let mut owners = vec![pair];
        unsafe { read_and_write(&mut *read, read_header, &mut owners) };
        assert_eq!(owners[0].read().value, 2);
        assert_eq!(owners[0].write().value, 11);

        // Completing one direction must not borrow the other one's live header.
        unsafe {
            let read = &mut *Op::<TestOp>::data_from_overlapped(read_header.cast());
            issue(read, read_header);
            (*write_header).Internal = 9;
            assert_eq!(read.value, 3);
            let write = &mut *Op::<TestOp>::data_from_overlapped(write_header.cast());
            issue(write, write_header);
            (*read_header).Internal = 8;
            assert_eq!(write.value, 12);
            assert_eq!((*read_header.cast::<Overlapped>()).udata, 1);
            assert_eq!((*write_header.cast::<Overlapped>()).udata, 2);
        }
        owners[0].read_mut().value += 1;
        assert_eq!(owners[0].read().value, 4);
        assert!(format!("{:?}", owners[0]).contains("OpPairBox"));
    }

    #[test]
    fn miri_op_pair_without_read_header() {
        struct CountDrop(std::rc::Rc<std::cell::Cell<usize>>, usize);

        impl Drop for CountDrop {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        #[inline(never)]
        fn read_and_write(read: &mut CountDrop, owners: &mut Vec<OpPairBox<CountDrop, TestOp>>) {
            read.1 += 1;
            owners.reserve(1024);
            let owner = &mut owners[0];
            let write_header = owner.write_overlapped();
            issue(owner.write_mut(), write_header);
            // The read borrow survives moving the owner and a nested write.
            read.1 += 1;
        }

        assert_eq!(
            mem::size_of::<OpPair<TestOp, CountDrop>>() + mem::size_of::<Overlapped>(),
            mem::size_of::<OpPair<TestOp, Op<CountDrop>>>()
        );

        let drops = std::rc::Rc::new(std::cell::Cell::new(0));
        let pair = OpPairBox::new(
            None,
            CountDrop(drops.clone(), 0),
            Overlapped::new(1, 2),
            TestOp { value: 10 },
        );
        assert!(!pair.has_read_header());
        assert!(pair.read_overlapped().is_null());
        let write_header = pair.write_overlapped();
        let read = pair.read_ptr();
        let mut owners = vec![pair];
        unsafe { read_and_write(&mut *read, &mut owners) };
        assert_eq!(owners[0].read().1, 2);
        assert_eq!(owners[0].write().value, 11);

        // The write completion recovers its payload from the untagged header.
        unsafe {
            let write = &mut *Op::<TestOp>::data_from_overlapped(write_header.cast());
            issue(write, write_header);
            assert_eq!(write.value, 12);
            assert_eq!((*write_header.cast::<Overlapped>()).udata, 2);
        }
        owners[0].read_mut().1 += 1;
        assert_eq!(owners[0].read().1, 3);
        drop(owners);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn miri_op_pair_drops_both_payloads() {
        struct CountDrop(std::rc::Rc<std::cell::Cell<usize>>);

        impl Drop for CountDrop {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let read = std::rc::Rc::new(std::cell::Cell::new(0));
        let write = std::rc::Rc::new(std::cell::Cell::new(0));
        let pair = OpPairBox::new(
            Some(Overlapped::new(1, 1)),
            CountDrop(read.clone()),
            Overlapped::new(1, 2),
            CountDrop(write.clone()),
        );
        let read_header = pair.read_overlapped();
        // The read header is inside, not at the start of, the allocation.
        unsafe { (*read_header).Internal = 1 };
        drop(pair);
        assert_eq!(read.get(), 1);
        assert_eq!(write.get(), 1);
    }
}
