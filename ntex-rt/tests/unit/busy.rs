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

/// STATEMENT: A turn is counted as long once it ends after running for at least 100 ms,
/// also when it ends the busy period.
///
/// HOW:
/// - Drive the clock with fixed timestamps: a 50 ms turn, a turn of exactly 100 ms, and a
///   101 ms turn that ends the busy period.
/// - Start another busy period with a 99 ms turn.
/// - Check the count after each turn ends.
#[test]
fn long_turns_are_counted_when_they_end() {
    let ms = Duration::from_millis;
    let clock = BusyClock::new();
    let start = Instant::now();
    clock.start(start);
    clock.next_turn(start + ms(50));
    assert_eq!(clock.long_turns(), 0);
    clock.next_turn(start + ms(150));
    assert_eq!(clock.long_turns(), 1);
    clock.stop(start + ms(251));
    assert_eq!(clock.long_turns(), 2);

    clock.start(start + ms(300));
    clock.stop(start + ms(399));
    assert_eq!(clock.long_turns(), 2);
}
