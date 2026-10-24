//! Allocation counts of filter chain conversions and local waiter operations.
use std::alloc::{GlobalAlloc, Layout, System};
use std::{cell::Cell, io};

use ntex::io::{FilterBuf, FilterLayer, Handle, Io, IoContext, IoStream};
use ntex::service::cfg::SharedCfg;

struct Counting;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let before = ALLOCS.with(Cell::get);
    let res = f();
    (res, ALLOCS.with(Cell::get) - before)
}

#[derive(Debug)]
struct Transport;

impl IoStream for Transport {
    fn start(self, _: IoContext) -> Box<dyn Handle> {
        Box::new(self)
    }
}

impl Handle for Transport {}

#[derive(Debug)]
struct Pass;

impl FilterLayer for Pass {
    fn process_read_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
        Ok(())
    }

    fn process_write_buf(&self, _: &FilterBuf<'_>) -> io::Result<()> {
        Ok(())
    }
}

#[ntex::test]
async fn filter_chain_conversions_allocate_only_the_new_filter() {
    let cfg: SharedCfg = SharedCfg::new("SRV").into();

    // warm up thread-local state
    drop(
        Io::new(Transport, cfg.clone())
            .add_filter(Pass)
            .map_filter(|f| f)
            .boxed(),
    );

    let io = Io::new(Transport, cfg.clone());
    let (io, n) = allocs(|| io.add_filter(Pass));
    assert_eq!(n, 1, "add_filter");
    let (io, n) = allocs(|| io.map_filter(|f| f));
    assert_eq!(n, 1, "map_filter");
    let (io, n) = allocs(|| io.seal());
    assert_eq!(n, 1, "seal");
    let (io, n) = allocs(|| io.seal());
    assert_eq!(n, 0, "seal of a sealed io");
    let (io, n) = allocs(|| io.boxed());
    assert_eq!(n, 0, "boxed of a sealed io");
    drop(io);

    let io = Io::new(Transport, cfg.clone()).add_filter(Pass);
    let (io, n) = allocs(|| io.boxed());
    assert_eq!(n, 1, "boxed");
    drop(io);
}

#[ntex::test]
async fn local_waiter_reuse_and_delivery_do_not_allocate() {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct Wrapped(Waker);

    impl Wake for Wrapped {
        fn wake(self: Arc<Self>) {
            self.0.wake_by_ref();
        }
    }

    let io = Io::new(Transport, SharedCfg::new("SRV"));
    let waiter = io.waiter(7);
    std::future::poll_fn(|cx| {
        let wrapped = Waker::from(Arc::new(Wrapped(cx.waker().clone())));
        let cycle = || {
            let mut native = Context::from_waker(cx.waker());
            let mut fallback = Context::from_waker(&wrapped);
            assert!(waiter.poll_ready(&mut native).is_pending());
            assert!(waiter.poll_ready(&mut native).is_pending());
            assert!(waiter.poll_ready(&mut fallback).is_pending());
            io.wake(7);
            assert!(waiter.poll_ready(&mut fallback).is_ready());
            assert!(waiter.poll_ready(&mut fallback).is_pending());
            assert!(waiter.poll_ready(&mut native).is_pending());
            io.wake(7);
            assert!(waiter.poll_ready(&mut native).is_ready());
        };
        cycle();
        let ((), count) = allocs(cycle);
        assert_eq!(count, 0, "warmed-up registration, replacement and wake");
        Poll::Ready(())
    })
    .await;
}
