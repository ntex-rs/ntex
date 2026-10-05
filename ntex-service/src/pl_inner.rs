use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::{cell, future::Future, pin::Pin, ptr, ptr::NonNull, rc::Rc, task::Context, task::Poll};

use crate::{Ctx, Service, ctx::WaitersRef, util::BoxFuture};

// ======================== PipelineApi ============================

pub(crate) struct PipelineApi<Req, Res, Err>(Rc<dyn PipelineInternalApi<Req, Res, Err>>);

impl<Req, Res, Err> PipelineApi<Req, Res, Err> {
    pub(crate) fn new<S, St>(s: S, st: St) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
        Req: 'static,
    {
        PipelineApi(Rc::new(PipelineInner {
            s,
            st,
            waiters: WaitersRef::new(),
            st_runtime: cell::UnsafeCell::new(RuntimeState::New),
            calls: CallCache::default(),
        }))
    }

    pub(crate) fn with(api: impl PipelineInternalApi<Req, Res, Err> + 'static) -> Self {
        Self(Rc::new(api))
    }
}

impl<Req, Res, Err> PipelineApi<Req, Res, Err> {
    pub(crate) fn reg(&self) -> u32 {
        self.0.reg()
    }

    pub(crate) fn unreg(&self, idx: u32) {
        self.0.unreg(idx);
    }

    pub(crate) fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>> {
        self.0.ready(idx)
    }

    pub(crate) fn call(&self, idx: u32, req: Req, ready: bool) -> CallFuture<'_, Result<Res, Err>> {
        self.0.call(idx, req, ready)
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>> {
        self.0.poll_ready(cx)
    }

    pub(crate) fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.0.poll_shutdown(cx)
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.0.is_shutdown()
    }
}

impl<Req, Res, Err> Clone for PipelineApi<Req, Res, Err> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

// ======================== PipelineInner ============================

struct PipelineInner<S: Service<St, Req>, St, Req> {
    s: S,
    st: St,
    st_runtime: cell::UnsafeCell<RuntimeState<S::Error>>,
    waiters: WaitersRef,
    calls: CallCache,
}

impl<S: Service<St, Req>, St, Req> Drop for PipelineInner<S, St, Req> {
    fn drop(&mut self) {
        // Readiness and shutdown futures borrow `s`, `st` and `waiters`, so they
        // must be dropped before the fields they reference
        *self.st_runtime.get_mut() = RuntimeState::Done;
    }
}

enum RuntimeState<E> {
    New,
    Readiness(BoxFuture<'static, Result<(), E>>),
    Shutdown(BoxFuture<'static, ()>),
    Done,
}

// ======================== PipelineInternalApi ============================

pub(crate) trait PipelineInternalApi<Req, Res, Err> {
    fn reg(&self) -> u32;

    fn unreg(&self, idx: u32);

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>>;

    fn call(&self, idx: u32, req: Req, ready: bool) -> CallFuture<'_, Result<Res, Err>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>>;

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()>;

    fn is_shutdown(&self) -> bool;
}

impl<S, St, Req> PipelineInternalApi<Req, S::Res, S::Error> for PipelineInner<S, St, Req>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    fn reg(&self) -> u32 {
        self.waiters.insert()
    }

    fn unreg(&self, index: u32) {
        self.waiters.remove(index);
    }

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), S::Error>> {
        Box::pin(async move {
            Ctx::<'_, S, St>::new(idx, &self.waiters, &self.st)
                .ready(&self.s)
                .await
        })
    }

    fn call(&self, idx: u32, req: Req, ready: bool) -> CallFuture<'_, Result<S::Res, S::Error>> {
        CallFuture::new_in(&self.calls, async move {
            if ready {
                Ctx::<'_, S, St>::new(idx, &self.waiters, &self.st)
                    .call(&self.s, req)
                    .await
            } else {
                Ctx::<'_, S, St>::new(idx, &self.waiters, &self.st)
                    .call_nowait(&self.s, req)
                    .await
            }
        })
    }

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        let st = unsafe { &mut *self.st_runtime.get() };
        match st {
            RuntimeState::New => {
                // SAFETY: `self` is heap allocated (`Rc<PipelineInner>`) and never moves.
                // `fut` is stored in `self.st_runtime`, which is reset before other
                // fields are dropped (see `Drop for PipelineInner`), so `pl` outlives `fut`.
                let pl = unsafe { &*(ptr::from_ref(self)) };
                let fut = Box::pin(CheckReadiness {
                    pl,
                    f: ready,
                    fut: None,
                });
                *st = RuntimeState::Readiness(fut);
                self.poll_ready(cx)
            }
            RuntimeState::Readiness(fut) => Pin::new(fut).poll(cx),
            RuntimeState::Shutdown(_) | RuntimeState::Done => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let st = unsafe { &mut *self.st_runtime.get() };
        match st {
            RuntimeState::New | RuntimeState::Readiness(_) => {
                // SAFETY: `self` is heap allocated (`Rc<PipelineInner>`) and never moves.
                // `fut` is stored in `self.st_runtime`, which is reset before other
                // fields are dropped (see `Drop for PipelineInner`), so `pl` outlives `fut`.
                let pl = unsafe { &*(ptr::from_ref(self)) };

                let fut = Box::pin(async move {
                    let ctx = Ctx::<'_, S, St>::new(0, &pl.waiters, &pl.st);
                    pl.s.shutdown(ctx).await;
                });
                *st = RuntimeState::Shutdown(fut);
                pl.waiters.shutdown();
                self.poll_shutdown(cx)
            }
            RuntimeState::Shutdown(fut) => {
                let res = Pin::new(fut).poll(cx);
                if res.is_ready() {
                    *st = RuntimeState::Done;
                }
                res
            }
            RuntimeState::Done => Poll::Ready(()),
        }
    }

    fn is_shutdown(&self) -> bool {
        self.waiters.is_shutdown()
    }
}

fn ready<S, St, Req>(
    pl: &'static PipelineInner<S, St, Req>,
) -> impl Future<Output = Result<(), S::Error>>
where
    S: Service<St, Req>,
{
    pl.s.ready(Ctx::<'_, S, St>::new(0, &pl.waiters, &pl.st))
}

struct CheckReadiness<S, St, Req, F, Fut>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    f: F,
    fut: Option<Fut>,
    pl: &'static PipelineInner<S, St, Req>,
}

impl<S, St, Req, F, Fut> Unpin for CheckReadiness<S, St, Req, F, Fut> where S: Service<St, Req> {}

impl<S, St, Req, F, Fut> Drop for CheckReadiness<S, St, Req, F, Fut>
where
    S: Service<St, Req>,
{
    fn drop(&mut self) {
        // future got dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.waiters.notify();
        }
    }
}

impl<S, St, Req, F, Fut> Future for CheckReadiness<S, St, Req, F, Fut>
where
    S: Service<St, Req>,
    F: Fn(&'static PipelineInner<S, St, Req>) -> Fut,
    Fut: Future<Output = Result<(), S::Error>>,
{
    type Output = Result<(), S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut();

        this.pl.waiters.run(0, cx, |cx| {
            if this.fut.is_none() {
                this.fut = Some((this.f)(this.pl));
            }
            let fut = this.fut.as_mut().unwrap();
            let result = unsafe { Pin::new_unchecked(fut) }.poll(cx);
            if result.is_ready() {
                let _ = this.fut.take();
            }
            result
        })
    }
}

// ======================== CallFuture ============================

/// Heap allocated service call future.
///
/// Futures created with [`CallFuture::new_in`] return their memory to a
/// [`CallCache`] on drop, so subsequent calls can reuse it.
pub(crate) struct CallFuture<'a, R> {
    fut: NonNull<dyn Future<Output = R> + 'a>,
    cache: Option<&'a CallCache>,
}

impl<'a, R> CallFuture<'a, R> {
    fn new_in<F>(cache: &'a CallCache, fut: F) -> Self
    where
        F: Future<Output = R> + 'a,
    {
        if size_of::<F>() == 0 {
            return Self::boxed(Box::pin(fut));
        }
        let ptr = cache.alloc(Layout::new::<F>()).cast::<F>();
        // SAFETY: `ptr` is valid for writes of `F`
        unsafe { ptr.as_ptr().write(fut) };
        Self {
            fut: ptr,
            cache: Some(cache),
        }
    }

    pub(crate) fn boxed(fut: BoxFuture<'a, R>) -> Self {
        // SAFETY: the future is not moved out of its allocation, it is
        // polled pinned and dropped in place
        let fut = Box::into_raw(unsafe { Pin::into_inner_unchecked(fut) });
        Self {
            // SAFETY: `Box::into_raw` never returns null
            fut: unsafe { NonNull::new_unchecked(fut) },
            cache: None,
        }
    }
}

impl<R> Drop for CallFuture<'_, R> {
    fn drop(&mut self) {
        // SAFETY: `fut` points to a valid future that is owned by `self`
        unsafe {
            if let Some(cache) = self.cache {
                let layout = Layout::for_value(self.fut.as_ref());
                ptr::drop_in_place(self.fut.as_ptr());
                cache.release(self.fut.cast(), layout);
            } else {
                drop(Box::from_raw(self.fut.as_ptr()));
            }
        }
    }
}

impl<R> Future for CallFuture<'_, R> {
    type Output = R;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        // SAFETY: the future is heap allocated and never moves until it is dropped
        unsafe { Pin::new_unchecked(&mut *self.fut.as_ptr()) }.poll(cx)
    }
}

/// Max number of call future blocks kept by a [`CallCache`].
const CALL_CACHE_MAX_BLOCKS: usize = 64;

/// Max total size of call future blocks kept by a [`CallCache`].
const CALL_CACHE_MAX_BYTES: usize = 256 * 1024;

/// Keeps memory of completed call futures.
///
/// Free blocks form a LIFO list, each block stores a pointer to the next one.
/// All blocks share the same layout.
pub(crate) struct CallCache {
    head: cell::Cell<Option<NonNull<u8>>>,
    layout: cell::Cell<Layout>,
    len: cell::Cell<usize>,
}

impl Default for CallCache {
    fn default() -> Self {
        Self {
            head: cell::Cell::new(None),
            layout: cell::Cell::new(Layout::new::<()>()),
            len: cell::Cell::new(0),
        }
    }
}

impl CallCache {
    fn alloc(&self, layout: Layout) -> NonNull<u8> {
        if layout == self.layout.get()
            && let Some(ptr) = self.head.get()
        {
            // SAFETY: cached blocks start with a pointer to the next free block
            let next = unsafe { ptr.cast::<Option<NonNull<u8>>>().as_ptr().read_unaligned() };
            self.head.set(next);
            self.len.set(self.len.get() - 1);
            ptr
        } else {
            // SAFETY: `layout` has non-zero size
            NonNull::new(unsafe { alloc(layout) }).unwrap_or_else(|| handle_alloc_error(layout))
        }
    }

    /// # Safety
    ///
    /// `ptr` must be allocated with `layout` by the global allocator and must not be used afterwards
    unsafe fn release(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout != self.layout.get() {
            self.clear();
            self.layout.set(layout);
        }

        let len = self.len.get();
        if layout.size() >= size_of::<Option<NonNull<u8>>>()
            && len < CALL_CACHE_MAX_BLOCKS
            && (len + 1) * layout.size() <= CALL_CACHE_MAX_BYTES
        {
            // SAFETY: the block is not used anymore and is large enough for a pointer
            unsafe {
                ptr.cast::<Option<NonNull<u8>>>()
                    .as_ptr()
                    .write_unaligned(self.head.get());
            }
            self.head.set(Some(ptr));
            self.len.set(len + 1);
        } else {
            // SAFETY: `ptr` was allocated with `layout`
            unsafe { dealloc(ptr.as_ptr(), layout) };
        }
    }

    fn clear(&self) {
        let layout = self.layout.get();
        while let Some(ptr) = self.head.get() {
            // SAFETY: cached blocks start with a pointer to the next free block
            // and were allocated with `layout`
            unsafe {
                self.head
                    .set(ptr.cast::<Option<NonNull<u8>>>().as_ptr().read_unaligned());
                dealloc(ptr.as_ptr(), layout);
            }
        }
        self.len.set(0);
    }

    #[cfg(test)]
    fn cached(&self) -> Option<NonNull<u8>> {
        self.head.get()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.len.get()
    }
}

impl Drop for CallCache {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::{future::pending, task::Waker};

    use super::*;
    use crate::Pipeline;

    struct Guard<'a>(&'a [usize]);

    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            assert_eq!(self.0.iter().sum::<usize>(), 3);
        }
    }

    struct Pending(Vec<usize>);

    impl Service<usize, ()> for Pending {
        type Res = ();
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), ()> {
            let _g = Guard(&self.0);
            pending().await
        }

        async fn call(&self, (): (), _: Ctx<'_, Self, usize>) -> Result<(), ()> {
            let _g = Guard(&self.0);
            pending().await
        }

        async fn shutdown(&self, _: Ctx<'_, Self, usize>) {
            let _g = Guard(&self.0);
            pending::<()>().await;
        }
    }

    #[test]
    fn miri_drop_with_pending_readiness() {
        let mut cx = Context::from_waker(Waker::noop());
        let pl = Pipeline::new(1, Pending(vec![1, 2]));
        assert!(pl.poll_ready(&mut cx).is_pending());
        drop(pl);
    }

    #[test]
    fn miri_drop_with_pending_shutdown() {
        let mut cx = Context::from_waker(Waker::noop());
        let pl = Pipeline::new(1, Pending(vec![1, 2]));
        assert!(pl.poll_shutdown(&mut cx).is_pending());
        assert!(pl.is_shutdown());
        drop(pl);
    }

    #[test]
    fn miri_drop_pending_call_after_pipeline() {
        let mut cx = Context::from_waker(Waker::noop());
        let pl = Pipeline::new(1, Pending(vec![1, 2]));
        let mut call = Box::pin(pl.call_nowait(()));
        drop(pl);
        assert!(call.as_mut().poll(&mut cx).is_pending());
        drop(call);
    }

    struct Echo;

    impl Service<(), Rc<usize>> for Echo {
        type Res = usize;
        type Error = ();

        async fn call(&self, req: Rc<usize>, _: Ctx<'_, Self, ()>) -> Result<usize, ()> {
            Ok(*req)
        }
    }

    fn echo() -> PipelineInner<Echo, (), Rc<usize>> {
        PipelineInner {
            s: Echo,
            st: (),
            st_runtime: cell::UnsafeCell::new(RuntimeState::New),
            waiters: WaitersRef::new(),
            calls: CallCache::default(),
        }
    }

    fn block<R>(fut: &CallFuture<'_, R>) -> NonNull<u8> {
        fut.fut.cast()
    }

    #[test]
    fn miri_call_future_reuses_memory() {
        let mut cx = Context::from_waker(Waker::noop());
        let pl = echo();

        let mut fut = pl.call(0, Rc::new(1), false);
        let b1 = block(&fut);
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(Ok(1)));
        assert_eq!(pl.calls.cached(), None);
        drop(fut);
        assert_eq!(pl.calls.cached(), Some(b1));

        let mut fut = pl.call(0, Rc::new(2), true);
        assert_eq!(block(&fut), b1);
        assert_eq!(pl.calls.cached(), None);

        // concurrent call allocates
        let fut2 = pl.call(0, Rc::new(3), false);
        let b2 = block(&fut2);
        assert_ne!(b2, b1);
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(Ok(2)));
        drop(fut);
        assert_eq!(pl.calls.cached(), Some(b1));
        drop(fut2);
        assert_eq!(pl.calls.cached(), Some(b2));
        assert_eq!(pl.calls.len(), 2);

        let fut = pl.call(0, Rc::new(4), false);
        assert_eq!(block(&fut), b2);
        let fut2 = pl.call(0, Rc::new(5), false);
        assert_eq!(block(&fut2), b1);
        assert_eq!(pl.calls.len(), 0);
    }

    #[test]
    fn miri_call_future_reuses_concurrent_memory() {
        let mut cx = Context::from_waker(Waker::noop());
        let pl = echo();

        let futs: Vec<_> = (0..4).map(|i| pl.call(0, Rc::new(i), false)).collect();
        let mut blocks: Vec<_> = futs.iter().map(block).collect();
        drop(futs);
        assert_eq!(pl.calls.len(), 4);

        let mut futs: Vec<_> = (0..4).map(|i| pl.call(0, Rc::new(i), false)).collect();
        let mut reused: Vec<_> = futs.iter().map(block).collect();
        blocks.sort();
        reused.sort();
        assert_eq!(blocks, reused);
        assert_eq!(pl.calls.len(), 0);

        for (i, fut) in futs.iter_mut().enumerate() {
            assert_eq!(Pin::new(fut).poll(&mut cx), Poll::Ready(Ok(i)));
        }
    }

    #[test]
    fn miri_call_cache_limits() {
        let cache = CallCache::default();
        let layout = Layout::new::<[usize; 4]>();
        let blocks: Vec<_> = (0..CALL_CACHE_MAX_BLOCKS + 2)
            .map(|_| cache.alloc(layout))
            .collect();
        for ptr in blocks {
            unsafe { cache.release(ptr, layout) };
        }
        assert_eq!(cache.len(), CALL_CACHE_MAX_BLOCKS);

        // blocks of a different layout replace cached blocks
        let layout = Layout::from_size_align(CALL_CACHE_MAX_BYTES / 3, 8).unwrap();
        let blocks: Vec<_> = (0..4).map(|_| cache.alloc(layout)).collect();
        for ptr in blocks {
            unsafe { cache.release(ptr, layout) };
        }
        assert_eq!(cache.len(), 3);

        // blocks smaller than a pointer are not cached
        let layout = Layout::new::<u8>();
        let ptr = cache.alloc(layout);
        unsafe { cache.release(ptr, layout) };
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.cached(), None);
    }

    #[test]
    fn miri_call_future_drops_request() {
        let mut cx = Context::from_waker(Waker::noop());
        let pl = echo();
        let req = Rc::new(1);

        let fut = pl.call(0, req.clone(), false);
        assert_eq!(Rc::strong_count(&req), 2);
        drop(fut);
        assert_eq!(Rc::strong_count(&req), 1);

        let mut fut = pl.call(0, req.clone(), false);
        assert_eq!(Rc::strong_count(&req), 2);
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(Ok(1)));
        assert_eq!(Rc::strong_count(&req), 1);
        drop(fut);
        assert_eq!(Rc::strong_count(&req), 1);
    }

    #[test]
    fn miri_boxed_call_future() {
        let mut cx = Context::from_waker(Waker::noop());
        let req = Rc::new(5);

        let fut = CallFuture::boxed(Box::pin(std::future::ready(req.clone())));
        assert_eq!(Rc::strong_count(&req), 2);
        drop(fut);
        assert_eq!(Rc::strong_count(&req), 1);

        let mut fut = CallFuture::boxed(Box::pin(std::future::ready(req.clone())));
        let Poll::Ready(res) = Pin::new(&mut fut).poll(&mut cx) else {
            panic!()
        };
        assert!(Rc::ptr_eq(&res, &req));
    }
}
