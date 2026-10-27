use super::*;

/// STATEMENT: Loom tracks overlapping mutable accesses throughout a shared-cell callback.
///
/// HOW:
/// - Hold one mutable access open while another thread attempts to write the same cell.
/// - Check that the model rejects the conflicting accesses with its write-tracking panic.
#[test]
#[should_panic(expected = "currently writing to cell")]
fn task_loom_shared_cell_access() {
    use loom::sync::atomic::Ordering;

    struct Shared(SharedCell<usize>);

    // Deliberately allow conflicting accesses to verify the tracking scope.
    unsafe impl Sync for Shared {}

    loom::model(|| {
        let cell = Arc::new(Shared(SharedCell::new(0)));
        let entered = Arc::new(loom::sync::atomic::AtomicBool::new(false));
        let (other, entered2) = (cell.clone(), entered.clone());
        let writer = loom::thread::spawn(move || {
            other.0.with_mut(|p| {
                entered2.store(true, Ordering::Release);
                loom::thread::yield_now();
                unsafe { *p = 1 };
            });
        });
        while !entered.load(Ordering::Acquire) {
            loom::thread::yield_now();
        }
        cell.0.with_mut(|p| unsafe { *p = 2 });
        writer.join().unwrap();
    });
}
