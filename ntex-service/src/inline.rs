//! Type-erased future with inline storage.
//!
//! [`InlineFuture`] hides a future's concrete type, like
//! `Pin<Box<dyn Future<Output = T>>>`, but stores small futures in an inline
//! slot of `WORDS` machine words instead of allocating. Futures that don't fit
//! are boxed, and the box pointer is stored in the slot.
//!
//! # Example
//!
//! ```
//! use ntex_service::dev::InlineFuture;
//!
//! fn double(value: u32) -> InlineFuture<'static, u32, 4> {
//!     InlineFuture::new(async move { value * 2 })
//! }
//!
//! # async fn example() {
//! assert_eq!(double(21).await, 42);
//! # }
//! ```
use std::marker::{PhantomData, PhantomPinned};
use std::task::{Context, Poll};
use std::{fmt, mem::MaybeUninit, pin::Pin, ptr};

/// Type-erased future stored inline when it fits into `WORDS` machine words.
///
/// A future is stored inline when its size is at most `max(WORDS, 1)` words
/// and its alignment is at most the alignment of `usize`. Other futures are
/// boxed. The slot always holds at least one word, so `WORDS = 0` boxes every
/// future larger than a pointer.
///
/// The future and its output may borrow data for `'a`. Like
/// `Pin<Box<dyn Future>>`, `InlineFuture` is neither `Send` nor `Sync`. Unlike
/// a box, it is not `Unpin`; pin it before polling, for example with
/// [`std::pin::pin!`] or by awaiting it.
pub struct InlineFuture<'a, T, const WORDS: usize> {
    vtable: &'a VTable<T>,
    slot: MaybeUninit<Slot<WORDS>>,
    _t: PhantomData<Pin<Box<dyn Future<Output = T> + 'a>>>,
    _p: PhantomPinned,
}

/// Inline storage of at least one word, the boxed path stores a pointer.
#[repr(C)]
union Slot<const WORDS: usize> {
    _words: [MaybeUninit<usize>; WORDS],
    _ptr: *mut (),
}

struct VTable<T> {
    poll: unsafe fn(*mut (), &mut Context<'_>) -> Poll<T>,
    drop: unsafe fn(*mut ()),
}

struct VTableOf<F>(PhantomData<F>);

impl<F: Future> VTableOf<F> {
    const VTABLE: VTable<F::Output> = VTable {
        poll: poll_raw::<F>,
        drop: drop_raw::<F>,
    };
}

unsafe fn poll_raw<F: Future>(ptr: *mut (), cx: &mut Context<'_>) -> Poll<F::Output> {
    // SAFETY: `ptr` points to an initialized `F` that is never moved out of
    // the pinned `InlineFuture`.
    unsafe { Pin::new_unchecked(&mut *ptr.cast::<F>()).poll(cx) }
}

unsafe fn drop_raw<F>(ptr: *mut ()) {
    // SAFETY: `ptr` points to an initialized `F` that is dropped only once.
    unsafe { ptr::drop_in_place(ptr.cast::<F>()) }
}

impl<'a, T, const WORDS: usize> InlineFuture<'a, T, WORDS> {
    /// Erases the type of `fut`, storing it inline if it fits and boxing it
    /// otherwise.
    pub fn new<F>(fut: F) -> Self
    where
        F: Future<Output = T> + 'a,
    {
        if Self::fits::<F>() {
            // SAFETY: `F` fits into the slot
            unsafe { Self::new_inline(fut) }
        } else {
            // SAFETY: a box is a single pointer, the slot holds at least one
            unsafe { Self::new_inline(Box::pin(fut)) }
        }
    }

    /// Returns `true` if a future of type `F` is stored inline.
    pub const fn fits<F>() -> bool {
        size_of::<F>() <= size_of::<Slot<WORDS>>() && align_of::<F>() <= align_of::<Slot<WORDS>>()
    }

    /// # Safety
    ///
    /// `F` must fit into the slot.
    unsafe fn new_inline<F>(fut: F) -> Self
    where
        F: Future<Output = T> + 'a,
    {
        debug_assert!(Self::fits::<F>());

        let mut this = InlineFuture {
            vtable: &VTableOf::<F>::VTABLE,
            slot: MaybeUninit::uninit(),
            _t: PhantomData,
            _p: PhantomPinned,
        };
        // SAFETY: the slot is large enough and aligned for `F`
        unsafe { this.slot.as_mut_ptr().cast::<F>().write(fut) };
        this
    }
}

impl<T, const WORDS: usize> Future for InlineFuture<'_, T, WORDS> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        // SAFETY: the inline future is pinned together with `self`.
        // The slot is accessed through a raw pointer, a `&mut` to it would
        // invalidate self-references of the stored future.
        unsafe {
            let this = self.get_unchecked_mut();
            (this.vtable.poll)((&raw mut this.slot).cast(), cx)
        }
    }
}

impl<T, const WORDS: usize> Drop for InlineFuture<'_, T, WORDS> {
    fn drop(&mut self) {
        // SAFETY: the slot holds the future described by the vtable
        unsafe { (self.vtable.drop)((&raw mut self.slot).cast()) }
    }
}

impl<T, const WORDS: usize> fmt::Debug for InlineFuture<'_, T, WORDS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineFuture").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, future::poll_fn, pin::pin, rc::Rc};

    use super::*;

    const WORDS: usize = 4;

    type Fut<'a, T> = InlineFuture<'a, T, WORDS>;

    struct DropGuard(Rc<Cell<usize>>);

    impl Drop for DropGuard {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    fn fits_of<F: Future, const W: usize>(_: &F) -> bool {
        InlineFuture::<'_, F::Output, W>::fits::<F>()
    }

    fn complete<T, const W: usize>(fut: InlineFuture<'_, T, W>) -> T {
        let mut fut = pin!(fut);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        loop {
            if let Poll::Ready(res) = fut.as_mut().poll(&mut cx) {
                return res;
            }
        }
    }

    async fn yield_once() {
        let mut yielded = false;
        poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    /// Ready future of `N + 1` words that checks its padding when polled.
    struct Padded<const N: usize> {
        pad: [usize; N],
        _guard: Option<DropGuard>,
    }

    impl<const N: usize> Padded<N> {
        fn new(guard: Option<DropGuard>) -> Self {
            Self {
                pad: [N; N],
                _guard: guard,
            }
        }
    }

    impl<const N: usize> Future for Padded<N> {
        type Output = bool;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<bool> {
            Poll::Ready(self.pad.iter().all(|w| *w == N))
        }
    }

    /// Ready future that requires 16-byte alignment.
    #[repr(align(16))]
    struct Aligned(u8);

    impl Future for Aligned {
        type Output = bool;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<bool> {
            Poll::Ready((&raw const *self).addr().is_multiple_of(16) && self.0 == 7)
        }
    }

    /// STATEMENT: Inline and boxed futures resolve to their output after pending polls.
    ///
    /// HOW:
    /// - Build an async block that fits the 4-word slot and one holding a 1 KiB buffer
    ///   that does not.
    /// - Poll both through `InlineFuture` with a no-op waker past one pending poll and
    ///   check their outputs.
    #[test]
    fn inline_and_boxed_futures_resolve() {
        let small = async {
            yield_once().await;
            1u32
        };
        assert!(fits_of::<_, WORDS>(&small));
        assert_eq!(complete(Fut::new(small)), 1);

        let large = async {
            let buf = [7u8; 1024];
            yield_once().await;
            u32::from(buf[1023])
        };
        assert!(!fits_of::<_, WORDS>(&large));
        assert_eq!(complete(Fut::new(large)), 7);
    }

    /// STATEMENT: Futures up to `WORDS` words are stored inline and larger ones are boxed.
    ///
    /// HOW:
    /// - Check that a `WORDS`-word `Padded` future fits the slot and a `WORDS + 1`-word
    ///   one does not.
    /// - Resolve both through `InlineFuture` and check that each sees its padding intact.
    #[test]
    fn slot_size_boundary() {
        assert_eq!(size_of::<Padded<3>>(), WORDS * size_of::<usize>());
        assert!(Fut::<bool>::fits::<Padded<3>>());
        assert!(!Fut::<bool>::fits::<Padded<4>>());

        assert!(complete(Fut::new(Padded::<3>::new(None))));
        assert!(complete(Fut::new(Padded::<4>::new(None))));
    }

    /// STATEMENT: A zero-word slot still holds one word, so only pointer-sized futures
    /// stay inline.
    ///
    /// HOW:
    /// - Check that `InlineFuture<_, 0>` is two words: the vtable and a one-word slot.
    /// - Check that a one-word `Padded<0>` fits and a two-word `Padded<1>` does not.
    /// - Resolve both and check that each sees its padding intact.
    #[test]
    fn zero_word_slot_holds_one_word() {
        type Zero = InlineFuture<'static, bool, 0>;

        assert_eq!(size_of::<Zero>(), 2 * size_of::<usize>());
        assert!(Zero::fits::<Padded<0>>());
        assert!(!Zero::fits::<Padded<1>>());

        assert!(complete(Zero::new(Padded::<0>::new(None))));
        assert!(complete(Zero::new(Padded::<1>::new(None))));
    }

    /// STATEMENT: Small futures aligned above `usize` are boxed rather than stored inline.
    ///
    /// HOW:
    /// - Check that a 16-byte-aligned `Aligned` future, smaller than the slot, does not fit.
    /// - Resolve it through `InlineFuture` and check that it was polled at an aligned
    ///   address with its value intact.
    #[test]
    fn over_aligned_futures_are_boxed() {
        assert!(size_of::<Aligned>() < WORDS * size_of::<usize>());
        assert!(align_of::<Aligned>() > align_of::<usize>());
        assert!(!Fut::<bool>::fits::<Aligned>());

        assert!(complete(Fut::new(Aligned(7))));
    }

    /// STATEMENT: Dropping a pending or unpolled future runs its destructors exactly once.
    ///
    /// HOW:
    /// - Capture a drop-counting guard in inline and boxed async blocks.
    /// - Drop one of each unpolled and another after a pending poll.
    /// - Check that the counter grows by one for every dropped future.
    #[test]
    fn dropping_runs_destructor_once() {
        fn make(cnt: &Rc<Cell<usize>>, large: bool) -> Fut<'static, ()> {
            let guard = DropGuard(cnt.clone());
            if large {
                let fut = async move {
                    let buf = [0u8; 1024];
                    yield_once().await;
                    drop((guard, buf));
                    unreachable!()
                };
                assert!(!fits_of::<_, WORDS>(&fut));
                Fut::new(fut)
            } else {
                let fut = async move {
                    yield_once().await;
                    drop(guard);
                    unreachable!()
                };
                assert!(fits_of::<_, WORDS>(&fut));
                Fut::new(fut)
            }
        }

        let cnt = Rc::new(Cell::new(0));
        let mut cx = Context::from_waker(std::task::Waker::noop());

        for (idx, large) in [false, true].into_iter().enumerate() {
            drop(make(&cnt, large));
            assert_eq!(cnt.get(), idx * 2 + 1);

            let mut fut = Box::pin(make(&cnt, large));
            assert!(fut.as_mut().poll(&mut cx).is_pending());
            assert_eq!(cnt.get(), idx * 2 + 1);
            drop(fut);
            assert_eq!(cnt.get(), idx * 2 + 2);
        }
    }

    /// STATEMENT: A completed future is dropped once, when the `InlineFuture` is dropped.
    ///
    /// HOW:
    /// - Wrap inline and boxed `Padded` futures holding drop-counting guards.
    /// - Poll each to `Ready` and check that its guard is still alive.
    /// - Drop each `InlineFuture` and check that the counter grows by exactly one.
    #[test]
    fn dropping_after_ready_runs_destructor_once() {
        let cnt = Rc::new(Cell::new(0));
        let mut cx = Context::from_waker(std::task::Waker::noop());

        let mut inline = Box::pin(Fut::new(Padded::<3>::new(Some(DropGuard(cnt.clone())))));
        let mut boxed = Box::pin(Fut::new(Padded::<4>::new(Some(DropGuard(cnt.clone())))));
        for fut in [&mut inline, &mut boxed] {
            assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(true));
        }
        assert_eq!(cnt.get(), 0);

        drop(inline);
        assert_eq!(cnt.get(), 1);
        drop(boxed);
        assert_eq!(cnt.get(), 2);
    }

    /// STATEMENT: An inline future that borrows its own state stays valid across polls.
    ///
    /// HOW:
    /// - Build an async block that keeps a reference to its local array across two
    ///   pending polls, and check that it fits a 16-word slot.
    /// - Pin it inside `InlineFuture`, resolve it, and check that reads through the
    ///   reference see the array's values (run under Miri to detect invalidation).
    #[test]
    fn self_referential_future_stays_valid() {
        let fut = async {
            let data = [3u8, 4];
            let r = &data;
            yield_once().await;
            let first = r[0];
            yield_once().await;
            u32::from(first) + u32::from(r[1])
        };
        assert!(fits_of::<_, 16>(&fut));
        assert_eq!(complete(InlineFuture::<'_, u32, 16>::new(fut)), 7);
    }

    /// STATEMENT: Futures borrowing non-`'static` data can be erased.
    ///
    /// HOW:
    /// - Erase inline and boxed async blocks that update a stack-local `Cell`
    ///   across a pending poll.
    /// - Resolve both and check their outputs and the `Cell` value.
    #[test]
    fn futures_borrowing_local_data() {
        let cell = Cell::new(1u32);
        let small: Fut<'_, u32> = Fut::new(async {
            yield_once().await;
            cell.set(cell.get() + 1);
            cell.get()
        });
        let large: Fut<'_, u32> = Fut::new(async {
            let buf = [5u8; 1024];
            yield_once().await;
            cell.set(cell.get() + u32::from(buf[1023]));
            cell.get()
        });

        assert_eq!(complete(small), 2);
        assert_eq!(complete(large), 7);
        assert_eq!(cell.get(), 7);
    }

    /// STATEMENT: The output may borrow non-`'static` data.
    ///
    /// HOW:
    /// - Erase inline and boxed async blocks whose output is a `&str` borrowed
    ///   from a stack-local `String`, across a pending poll.
    /// - Resolve both and check that the outputs point into that `String`.
    #[test]
    fn output_borrowing_local_data() {
        let data = String::from("inline");
        let small = async {
            yield_once().await;
            &data[..2]
        };
        let large = async {
            let buf = [0u8; 1024];
            yield_once().await;
            &data[usize::from(buf[1023])..]
        };
        assert!(fits_of::<_, WORDS>(&small));
        assert!(!fits_of::<_, WORDS>(&large));

        assert_eq!(complete::<&str, WORDS>(Fut::new(small)), "in");
        assert_eq!(
            complete::<&str, WORDS>(Fut::new(large)).as_ptr(),
            data.as_ptr()
        );
    }
}
