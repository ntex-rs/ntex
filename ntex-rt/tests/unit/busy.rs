use super::*;

/// STATEMENT: Publishing a completed busy period never decreases a duration already observed.
///
/// HOW:
/// - Read the busy clock after the stop timestamp but before publishing the stop.
/// - Verify monotonic reads and an exact accumulated total after the next busy period.
#[test]
fn read_before_stop_is_not_undone() {
    let clock = BusyClock::new();
    let start = Instant::now();
    clock.start(start);
    // the runtime thread reads the clock for `stop`, then a reader reads
    // it before the state is published
    let stop = start + Duration::from_millis(1);
    while Instant::now() < stop + Duration::from_millis(2) {
        std::hint::spin_loop();
    }
    let read = clock.busy_duration();
    clock.stop(stop);
    assert!(clock.busy_duration() >= read);

    // the published total stays exact, the next period catches up
    assert!(Duration::from_nanos(clock.read()) < read);
    let next = Instant::now();
    clock.start(next);
    clock.stop(next + Duration::from_millis(5));
    assert_eq!(clock.busy_duration(), Duration::from_millis(6));
}
