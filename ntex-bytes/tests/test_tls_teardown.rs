//! Buffers released or allocated by thread-local destructors, after the
//! thread-local page caches may already be destroyed.
use std::{any::Any, cell::RefCell, thread};

use ntex_bytes::{BytePageSize, BytePages};

thread_local! {
    static HOLD: RefCell<Vec<Box<dyn Any>>> = const { RefCell::new(Vec::new()) };
}

struct AllocOnDrop;

impl Drop for AllocOnDrop {
    fn drop(&mut self) {
        let mut pages = BytePages::new(BytePageSize::Size4);
        pages.extend_from_slice(&[1; 100]);
        assert_eq!(pages.freeze(), &[1; 100][..]);
    }
}

fn sized_page() -> Box<dyn Any> {
    let mut pages = BytePages::new(BytePageSize::Size4);
    pages.extend_from_slice(b"hello");
    Box::new(pages.take().unwrap())
}

fn pages() -> Box<dyn Any> {
    let mut pages = BytePages::new(BytePageSize::Size4);
    pages.extend_from_slice(&[2; 5000]);
    Box::new(pages)
}

fn alloc_on_drop() -> Box<dyn Any> {
    drop(sized_page());
    drop(pages());
    Box::new(AllocOnDrop)
}

/// Runs `f` on a new thread and keeps its result alive until thread exit.
///
/// With `hold_first` the holder is registered before the caches, so it is
/// dropped after them, otherwise before them.
fn run(hold_first: bool, f: fn() -> Box<dyn Any>) {
    thread::spawn(move || {
        if hold_first {
            HOLD.with(|_| ());
        }
        let item = f();
        HOLD.with(|h| h.borrow_mut().push(item));
    })
    .join()
    .unwrap();
}

#[test]
fn drop_sized_page_on_thread_exit() {
    run(true, sized_page);
    run(false, sized_page);
}

#[test]
fn drop_pages_on_thread_exit() {
    run(true, pages);
    run(false, pages);
}

#[test]
fn alloc_pages_on_thread_exit() {
    run(true, alloc_on_drop);
    run(false, alloc_on_drop);
}
