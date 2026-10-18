//! Allocation counts of filter chain conversions.
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
