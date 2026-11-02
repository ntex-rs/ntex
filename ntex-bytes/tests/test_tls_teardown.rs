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

/// STATEMENT: A sized page can be released at thread exit before or after its thread-local cache is destroyed.
///
/// HOW:
/// - Hold a `hello` page in thread-local storage with both holder registration orders and require each thread to join successfully.
#[test]
fn drop_sized_page_on_thread_exit() {
    run(true, sized_page);
    run(false, sized_page);
}

/// STATEMENT: Multi-page buffers can be released at thread exit in either cache-destruction order.
///
/// HOW:
/// - Hold a 5,000-byte `BytePages` buffer in thread-local storage before and after cache registration and join both threads successfully.
#[test]
fn drop_pages_on_thread_exit() {
    run(true, pages);
    run(false, pages);
}

/// STATEMENT: Thread-local destructors can allocate and use pages even after page caches are destroyed.
///
/// HOW:
/// - Register `AllocOnDrop` in both holder orders and join each thread; its destructor allocates and freezes 100 bytes and checks the contents.
#[test]
fn alloc_pages_on_thread_exit() {
    run(true, alloc_on_drop);
    run(false, alloc_on_drop);
}
