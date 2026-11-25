use std::alloc::{self, Layout};
use std::task::{Context, Poll};
use std::{cell::Cell, future::Future, future::poll_fn, marker::PhantomData, mem, pin, pin::Pin};
use std::{mem::MaybeUninit, ptr, ptr::NonNull};

use crate::{Ctx, Service, readiness::Generation};

pub(crate) type BoxFuture<'a, R> = Pin<Box<dyn Future<Output = R> + 'a>>;

/// A heap-allocated, type-erased pipeline call or pipeline create future.
///
/// Experimental: pipeline calls keep the blocks of finished call futures in a
/// per-pipeline [`CallPool`] for reuse, and `PipelineFactory` does the same for
/// create futures. This may change or be reverted.
///
/// Unlike [`BoxFuture`], dropping the future returns its block to the
/// [`CallPool`] of its owner.
pub(crate) struct CallFuture<'a, R> {
    ptr: NonNull<u8>,
    // A reference would require `R: 'a`, which `BoxFuture` does not
    vtable: NonNull<CallVTable<R>>,
    // Same auto traits, variance and drop check as `BoxFuture`
    _marker: PhantomData<BoxFuture<'a, R>>,
}

struct CallVTable<R> {
    poll: unsafe fn(NonNull<u8>, &mut Context<'_>) -> Poll<R>,
    release: unsafe fn(NonNull<u8>),
}

impl<'a, R> CallFuture<'a, R> {
    /// Builds the future with `f` in a block taken from the pool of `owner`.
    ///
    /// The future keeps `owner`. Dropping the future returns the block to the
    /// pool of `owner` before `owner` is dropped. Taking the block before `f`
    /// runs lets the future be written directly into the block instead of
    /// being built on the stack and copied.
    #[inline]
    pub(crate) fn pooled<F, O>(owner: O, f: impl FnOnce(&O) -> F) -> Self
    where
        F: Future<Output = R> + 'a,
        O: PoolOwner + 'a,
    {
        let block = owner
            .pool()
            .take(const { block_layout::<WithOwner<F, O>>() });
        let fut = f(&owner);
        // SAFETY: the block fits `WithOwner<F, O>`
        unsafe {
            block
                .ptr
                .cast::<WithOwner<F, O>>()
                .write(WithOwner { fut, owner });
        };
        Self {
            ptr: block.into_ptr(),
            vtable: NonNull::from(
                const {
                    &CallVTable {
                        poll: poll_block::<WithOwner<F, O>>,
                        release: release_block::<F, O>,
                    }
                },
            ),
            _marker: PhantomData,
        }
    }
}

impl<R> Future for CallFuture<'_, R> {
    type Output = R;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        // SAFETY: `vtable` is a constant, and `ptr` holds the future it was built for
        unsafe { (self.vtable.as_ref().poll)(self.ptr, cx) }
    }
}

impl<R> Drop for CallFuture<'_, R> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `vtable` is a constant, and `ptr` holds the future it was built for
        unsafe { (self.vtable.as_ref().release)(self.ptr) }
    }
}

/// # Safety
///
/// `ptr` must hold a pinned `F`.
unsafe fn poll_block<F: Future>(ptr: NonNull<u8>, cx: &mut Context<'_>) -> Poll<F::Output> {
    // SAFETY: guaranteed by the caller
    unsafe { Pin::new_unchecked(ptr.cast::<F>().as_mut()) }.poll(cx)
}

/// # Safety
///
/// `ptr` must hold a `WithOwner<F, O>` in a block built by [`CallFuture::pooled`].
unsafe fn release_block<F, O: PoolOwner>(ptr: NonNull<u8>) {
    // Releases the block and the owner even if dropping the future panics
    struct Release<O: PoolOwner> {
        ptr: NonNull<u8>,
        owner: *mut O,
        layout: Layout,
    }

    impl<O: PoolOwner> Drop for Release<O> {
        fn drop(&mut self) {
            // SAFETY: `owner` is initialized and is not used after this read
            let owner = unsafe { self.owner.read() };
            // SAFETY: the block was allocated with `layout`, and the future in it
            // has been dropped. The pool outlives `owner`, which is dropped last.
            unsafe { owner.pool().put(self.ptr, self.layout) };
        }
    }

    let fut = ptr.cast::<WithOwner<F, O>>().as_ptr();
    let _release = Release {
        ptr,
        // SAFETY: guaranteed by the caller
        owner: unsafe { &raw mut (*fut).owner },
        layout: const { block_layout::<WithOwner<F, O>>() },
    };
    // SAFETY: guaranteed by the caller
    unsafe { ptr::drop_in_place(&raw mut (*fut).fut) };
}

/// Provides the pool that a pooled call future returns its block to.
pub(crate) trait PoolOwner {
    fn pool(&self) -> &CallPool;
}

const POOL_CAP: usize = 4;

/// Blocks of dropped call futures, reused by later calls.
///
/// A sequential caller reuses one block. Up to `POOL_CAP` blocks are kept,
/// and all of them have the layout of the last returned block.
pub(crate) struct CallPool {
    blocks: [Cell<NonNull<u8>>; POOL_CAP],
    len: Cell<usize>,
    layout: Cell<Layout>,
}

impl CallPool {
    pub(crate) fn new() -> Self {
        Self {
            blocks: [const { Cell::new(NonNull::dangling()) }; POOL_CAP],
            len: Cell::new(0),
            layout: Cell::new(Layout::new::<()>()),
        }
    }

    /// Takes a block of `layout`, or allocates one if the pool has none.
    #[inline]
    fn take(&self, layout: Layout) -> Block {
        let len = self.len.get();
        if len > 0 && self.layout.get() == layout {
            self.len.set(len - 1);
            Block {
                ptr: self.blocks[len - 1].get(),
                layout,
            }
        } else {
            Block::alloc(layout)
        }
    }

    /// Keeps a block for a later call, or frees it if the pool cannot keep it.
    ///
    /// # Safety
    ///
    /// `ptr` must be a block allocated with `layout` and must not be used again.
    #[inline]
    unsafe fn put(&self, ptr: NonNull<u8>, layout: Layout) {
        let len = self.len.get();
        if len < POOL_CAP && (len == 0 || self.layout.get() == layout) {
            self.blocks[len].set(ptr);
            self.layout.set(layout);
            self.len.set(len + 1);
        } else {
            drop(Block { ptr, layout });
        }
    }
}

impl Drop for CallPool {
    fn drop(&mut self) {
        let layout = self.layout.get();
        for block in &self.blocks[..self.len.get()] {
            drop(Block {
                ptr: block.get(),
                layout,
            });
        }
    }
}

/// An allocated block, freed on drop.
struct Block {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Block {
    fn alloc(layout: Layout) -> Self {
        // SAFETY: `block_layout` never returns a zero-sized layout
        let ptr = unsafe { alloc::alloc(layout) };
        match NonNull::new(ptr) {
            Some(ptr) => Self { ptr, layout },
            None => alloc::handle_alloc_error(layout),
        }
    }

    fn into_ptr(self) -> NonNull<u8> {
        let ptr = self.ptr;
        mem::forget(self);
        ptr
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: the block was allocated with `layout`
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// Returns the layout of a block that holds `F` at its start.
///
/// A future whose size fills a mimalloc size class that is not a power of two
/// is padded, see [`misses_aligned_fast_path`].
const fn block_layout<F>() -> Layout {
    assert!(size_of::<F>() > 0, "call futures are never zero-sized");
    if needs_padding::<F>() {
        Layout::new::<Padded<F>>()
    } else {
        Layout::new::<F>()
    }
}

/// Adds trailing padding to `F`, which moves its block to the next size class.
#[repr(C)]
struct Padded<F> {
    _fut: F,
    _pad: MaybeUninit<[u8; 8]>,
}

/// Returns `true` if a box of `F` always misses mimalloc's aligned fast path
/// and a box of `Padded<F>` does not.
const fn needs_padding<F>() -> bool {
    misses_aligned_fast_path(size_of::<F>(), align_of::<F>())
        && !misses_aligned_fast_path(size_of::<Padded<F>>(), align_of::<Padded<F>>())
}

/// Returns `true` if allocating `size` bytes aligned to `align` never takes
/// mimalloc's aligned fast path.
///
/// The `mimalloc` crate allocates every layout with `mi_malloc_aligned`. Its
/// fast path reuses a free block from the size class of `size`. Otherwise,
/// unless the class size is a power of two, it over-allocates `size + align - 1`
/// bytes. When `size` fills a class, that over-allocation comes from the next
/// class, so the class of `size` never gets free blocks and every allocation
/// takes the slow path.
const fn misses_aligned_fast_path(size: usize, align: usize) -> bool {
    // `MI_SMALL_SIZE_MAX`, larger sizes never take the fast path
    const SMALL_MAX: usize = 128 * size_of::<usize>();

    if align <= 1 || align > size || size > SMALL_MAX {
        return false;
    }
    let class = mi_class_size(size);
    !class.is_power_of_two() && size + align - 1 > class
}

/// Returns the block size of the mimalloc size class for `size`, see `mi_bin`.
const fn mi_class_size(size: usize) -> usize {
    const WORD: usize = size_of::<usize>();

    let words = size.div_ceil(WORD);
    if words == 0 {
        WORD
    } else if words <= 8 {
        words * WORD
    } else {
        // Four classes per power of two
        let step = 1 << ((words - 1).ilog2() - 2);
        words.next_multiple_of(step) * WORD
    }
}

pin_project_lite::pin_project! {
    /// Keeps `owner` alive until `fut` is dropped.
    pub(crate) struct WithOwner<F, O> {
        // Declared first so it is dropped before `owner`.
        #[pin]
        pub(crate) fut: F,
        pub(crate) owner: O,
    }
}

impl<F: Future, O> Future for WithOwner<F, O> {
    type Output = F::Output;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

pin_project_lite::pin_project! {
    /// Maps the output of `fut` with `f`.
    pub struct MapOutput<F, M> {
        #[pin]
        fut: F,
        f: Option<M>,
    }
}

impl<F, M, O> Future for MapOutput<F, M>
where
    F: Future,
    M: FnOnce(F::Output) -> O,
{
    type Output = O;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<O> {
        let this = self.project();
        let output = std::task::ready!(this.fut.poll(cx));
        let f = this.f.take().expect("`MapOutput` polled after completion");
        Poll::Ready(f(output))
    }
}

/// Maps the output of `fut` with `f`, without an `async` wrapper.
pub fn map_output<F, M, O>(fut: F, f: M) -> MapOutput<F, M>
where
    F: Future,
    M: FnOnce(F::Output) -> O,
{
    MapOutput { fut, f: Some(f) }
}

pub(crate) fn shutdown<S, St, A, B, RA, RB>(
    svc1: &A,
    svc2: &B,
    ctx: Ctx<'_, S, St>,
) -> impl Future<Output = ()>
where
    A: Service<St, RA>,
    B: Service<St, RB>,
{
    join(ctx.shutdown(svc1), ctx.shutdown(svc2))
}

pub(crate) fn ready<S, St, A, B, RA, RB>(
    svc1: &A,
    svc2: &B,
    ctx: Ctx<'_, S, St>,
) -> impl Future<Output = Result<(), A::Error>>
where
    A: Service<St, RA>,
    B: Service<St, RB, Error = A::Error>,
{
    map_output(
        ready_join(
            ctx,
            move |ctx| ctx.ready_generation(svc1),
            move |ctx| ctx.ready_generation(svc2),
        ),
        |res| res.map(|_| ()),
    )
}

/// Polls both futures concurrently until both complete.
pub async fn join<A, B>(fut1: A, fut2: B)
where
    A: Future<Output = ()>,
    B: Future<Output = ()>,
{
    let mut fut1 = pin::pin!(fut1);
    let mut fut2 = pin::pin!(fut2);

    let mut ready1 = false;
    let mut ready2 = false;

    poll_fn(move |cx| {
        if !ready1 && Pin::new(&mut fut1).poll(cx).is_ready() {
            ready1 = true;
        }
        if !ready2 && Pin::new(&mut fut2).poll(cx).is_ready() {
            ready2 = true;
        }
        if ready1 && ready2 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// Waits until both readiness checks complete without an intervening invalidation.
///
/// A completed check is restarted when the scope is invalidated before the
/// other one completes. Returns the generation at which both are ready.
/// Both checks are created from a copy of `ctx`.
#[allow(clippy::manual_async_fn)]
pub fn ready_join<'a, S, St, E, F1, Fut1, F2, Fut2>(
    ctx: Ctx<'a, S, St>,
    f1: F1,
    f2: F2,
) -> impl Future<Output = Result<Generation, E>>
where
    F1: Fn(Ctx<'a, S, St>) -> Fut1,
    Fut1: Future<Output = Result<Generation, E>>,
    F2: Fn(Ctx<'a, S, St>) -> Fut2,
    Fut2: Future<Output = Result<Generation, E>>,
{
    // An `async fn` would store its arguments twice.
    async move {
        let mut fut1 = pin::pin!(Some(f1(ctx)));
        let mut fut2 = pin::pin!(Some(f2(ctx)));
        let scope = ctx.scope();
        let mut generation1 = scope.generation();
        let mut generation2 = scope.generation();

        poll_fn(|cx| {
            if is_idle(&mut fut1) && !scope.is_current(generation1) {
                fut1.set(Some(f1(ctx)));
            }
            if let Some(fut) = fut1.as_mut().as_pin_mut()
                && let Poll::Ready(generation) = fut.poll(cx)?
            {
                generation1 = generation;
                fut1.set(None);
            }

            if is_idle(&mut fut2) && !scope.is_current(generation2) {
                fut2.set(Some(f2(ctx)));
            }
            if let Some(fut) = fut2.as_mut().as_pin_mut()
                && let Poll::Ready(generation) = fut.poll(cx)?
            {
                generation2 = generation;
                fut2.set(None);
            }

            let current1 = scope.is_current(generation1);
            let current2 = scope.is_current(generation2);
            if is_idle(&mut fut1) && is_idle(&mut fut2) && current1 && current2 {
                Poll::Ready(Ok(generation1))
            } else {
                // A later poll or future drop can invalidate a completed sibling.
                // Retain pending work, but schedule another check for the dirty child.
                if (is_idle(&mut fut1) && !current1) || (is_idle(&mut fut2) && !current2) {
                    cx.waker().wake_by_ref();
                }
                Poll::Pending
            }
        })
        .await
    }
}

/// Checks one field's readiness and maps its error.
pub fn ready_field<S, St, T, Req, E>(
    ctx: Ctx<'_, S, St>,
    svc: &T,
    map_err: impl FnOnce(T::Error) -> E,
) -> impl Future<Output = Result<Generation, E>>
where
    T: Service<St, Req>,
{
    map_output(ctx.ready_generation(svc), |res| res.map_err(map_err))
}

// `Option::is_none()` would take `&Option<F>`, which retags the whole future
// and invalidates borrows it holds into itself.
fn is_idle<F>(fut: &mut Pin<&mut Option<F>>) -> bool {
    fut.as_mut().as_pin_mut().is_none()
}

#[cfg(test)]
#[path = "../tests/unit/util.rs"]
pub(crate) mod tests;
