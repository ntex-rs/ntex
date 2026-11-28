use super::*;

/// STATEMENT: Histogram ranges agree with bucket selection at every boundary.
///
/// HOW:
/// - Check representative durations, including zero and the largest duration.
/// - Verify each lower bound, exclusive upper bound, and value immediately below it.
#[test]
fn histogram_buckets() {
    let bucket = |us| AtomicHistogram::bucket(Duration::from_micros(us));
    assert_eq!(bucket(0), 0);
    assert_eq!(bucket(15), 0);
    assert_eq!(bucket(16), 1);
    assert_eq!(bucket(31), 1);
    assert_eq!(bucket(32), 2);
    assert_eq!(bucket(262_143), 14);
    assert_eq!(bucket(262_144), 15);
    assert_eq!(AtomicHistogram::bucket(Duration::MAX), 15);

    for i in 0..HISTOGRAM_BUCKETS {
        let (lower, upper) = Histogram::bucket_range(i);
        assert_eq!(AtomicHistogram::bucket(lower), i);
        if let Some(upper) = upper {
            assert_eq!(AtomicHistogram::bucket(upper), i + 1);
            assert_eq!(
                AtomicHistogram::bucket(upper.checked_sub(Duration::from_nanos(1)).unwrap()),
                i
            );
        }
    }
    assert_eq!(Histogram::bucket_range(15).1, None);
}

/// STATEMENT: Histogram deltas contain only new samples and merge back into the current snapshot.
///
/// HOW:
/// - Take snapshots before and after recording samples in two buckets.
/// - Check delta counts and reconstruct the current snapshot by merging.
#[test]
fn histogram_delta() {
    let h = AtomicHistogram::default();
    h.record(Duration::from_micros(1));
    let prev = h.snapshot();
    h.record(Duration::from_micros(1));
    h.record(Duration::from_millis(1));
    let cur = h.snapshot();
    assert_eq!(cur.count(), 3);

    let delta = cur.delta(&prev);
    assert_eq!(delta.count(), 2);
    assert_eq!(delta.buckets()[0], 1);
    assert_eq!(
        delta.buckets()[AtomicHistogram::bucket(Duration::from_millis(1))],
        1
    );
    assert_eq!(prev.merge(&delta), cur);
}

/// STATEMENT: Latency metrics expose configured sampling state and the latest recorded sample.
///
/// HOW:
/// - Check disabled and enabled-but-empty metrics.
/// - Record a sample and verify its interval, duration, timestamp, and histogram count.
#[test]
fn latency_stats() {
    let stats = LatencyStats::default();
    assert_eq!(stats.metrics(), None);

    stats.interval.store(100, Ordering::Relaxed);
    let m = stats.metrics().unwrap();
    assert_eq!((m.last_sample, m.histogram.count()), (None, 0));

    let now = Instant::now();
    stats.record(Duration::from_millis(2), now);
    let m = stats.metrics().unwrap();
    assert_eq!(m.interval, Duration::from_nanos(100));
    assert_eq!(m.last, Duration::from_millis(2));
    assert_eq!(m.last_sample, Some(now));
    assert_eq!(m.histogram.count(), 1);
}

/// STATEMENT: Stall detection reports threshold crossings and recovery without duplicate events.
///
/// HOW:
/// - Advance explicit timestamps across the interval-plus-threshold boundary.
/// - Record recovery, verify cumulative metrics, and trigger a second distinct stall.
#[test]
fn stall_check() {
    let ms = Duration::from_millis;
    let threshold = ms(20);
    let stats = LatencyStats::default();
    let t0 = Instant::now();
    assert_eq!(stats.check(t0 + ms(100), threshold, None), Stall::None);

    stats.base.set(t0).unwrap();
    stats.interval.store(as_nanos(ms(10)), Ordering::Relaxed);
    assert_eq!(stats.check(t0 + ms(30), threshold, None), Stall::None);
    assert_eq!(
        stats.check(t0 + ms(31), threshold, None),
        Stall::Started(ms(31))
    );
    assert_eq!(stats.check(t0 + ms(50), threshold, None), Stall::None);
    let m = stats.metrics().unwrap();
    assert_eq!((m.stalls, m.stall_duration, m.stalled), (1, ms(0), true));

    stats.record(ms(90), t0 + ms(100));
    assert_eq!(
        stats.check(t0 + ms(101), threshold, None),
        Stall::Recovered(ms(90))
    );
    assert_eq!(stats.check(t0 + ms(120), threshold, None), Stall::None);
    let m = stats.metrics().unwrap();
    assert_eq!((m.stalls, m.stall_duration, m.stalled), (1, ms(90), false));

    assert_eq!(
        stats.check(t0 + ms(131), threshold, None),
        Stall::Started(ms(31))
    );
    assert_eq!(stats.metrics().unwrap().stalls, 2);
}

/// STATEMENT: Stalls are measured from the deadline of the pending sample, not the nominal interval.
///
/// HOW:
/// - Set a pending deadline past `interval`, as the wheel rounds it up.
/// - Check that time past `interval + threshold` but within the deadline plus threshold is not a stall.
/// - Cross the deadline plus threshold and verify recovery reports the sample's lateness.
/// - Record a late sample and check the stale deadline does not start a new stall.
#[test]
fn stall_check_deadline() {
    let ms = Duration::from_millis;
    let threshold = ms(20);
    let stats = LatencyStats::default();
    let t0 = Instant::now();
    stats.base.set(t0).unwrap();
    stats.interval.store(as_nanos(ms(10)), Ordering::Relaxed);

    stats.expect(t0 + ms(40));
    assert_eq!(stats.check(t0 + ms(55), threshold, None), Stall::None);
    assert_eq!(
        stats.check(t0 + ms(61), threshold, None),
        Stall::Started(ms(61))
    );
    stats.record(ms(30), t0 + ms(70));
    assert_eq!(
        stats.check(t0 + ms(71), threshold, None),
        Stall::Recovered(ms(30))
    );

    stats.record(ms(60), t0 + ms(100));
    assert_eq!(stats.check(t0 + ms(125), threshold, None), Stall::None);
    assert_eq!(
        stats.check(t0 + ms(131), threshold, None),
        Stall::Started(ms(31))
    );
}

/// STATEMENT: Deferred stall checks ignore an idle arbiter and bound lateness by the running turn.
///
/// HOW:
/// - Enable deferred sampling with a deadline at 10 ms and a 20 ms threshold.
/// - Check an idle arbiter long past the deadline, and a 20 ms turn, are not stalled.
/// - Check a 25 ms turn starts a stall aged by the turn, and the sample taken after
///   it recovers with the duration measured from the turn start.
/// - Check a turn that started before the next deadline is late since the deadline.
#[test]
fn stall_check_deferred() {
    let ms = Duration::from_millis;
    let threshold = ms(20);
    let stats = LatencyStats::default();
    let t0 = Instant::now();
    stats.base.set(t0).unwrap();
    stats.interval.store(as_nanos(ms(10)), Ordering::Relaxed);
    stats.deferred.store(true, Ordering::Relaxed);
    stats.expect(t0 + ms(10));

    assert_eq!(stats.check(t0 + ms(500), threshold, None), Stall::None);
    assert_eq!(
        stats.check(t0 + ms(500), threshold, Some(ms(20))),
        Stall::None
    );
    assert_eq!(
        stats.check(t0 + ms(500), threshold, Some(ms(25))),
        Stall::Started(ms(25))
    );
    stats.record(ms(45), t0 + ms(520));
    assert_eq!(
        stats.check(t0 + ms(521), threshold, None),
        Stall::Recovered(ms(45))
    );

    // the turn started at 460 ms, the deadline at 530 ms
    stats.expect(t0 + ms(530));
    assert_eq!(
        stats.check(t0 + ms(560), threshold, Some(ms(100))),
        Stall::Started(ms(40))
    );
    stats.record(ms(70), t0 + ms(600));
    assert_eq!(
        stats.check(t0 + ms(601), threshold, None),
        Stall::Recovered(ms(70))
    );
    assert!(stats.metrics().unwrap().deferred);
}

/// STATEMENT: No stall is reported before the time returned by `stall_at`, and an arbiter
/// that keeps running is stalled right after it.
///
/// HOW:
/// - Check an arbiter without sampling has no stall time.
/// - With timer sampling and a 40 ms deadline, check the stall time is the deadline plus
///   the 20 ms threshold, `check` reports the stall only after it, and a stalled arbiter
///   is due at once.
/// - With deferred sampling, check an idle arbiter before its deadline stalls at the
///   deadline plus the threshold, and an idle arbiter past it a threshold after `now`.
/// - Check a turn that started after the deadline stalls a threshold after its start, and
///   one that started before it a threshold after the deadline.
#[test]
fn stall_at() {
    let ms = Duration::from_millis;
    let threshold = ms(20);
    let t0 = Instant::now();
    let stats = |deferred| {
        let stats = LatencyStats::default();
        stats.base.set(t0).unwrap();
        stats.interval.store(as_nanos(ms(10)), Ordering::Relaxed);
        stats.deferred.store(deferred, Ordering::Relaxed);
        stats.expect(t0 + ms(40));
        stats
    };
    assert_eq!(LatencyStats::default().stall_at(t0, threshold, None), None);

    let s = stats(false);
    assert_eq!(s.stall_at(t0 + ms(5), threshold, None), Some(t0 + ms(60)));
    assert_eq!(s.check(t0 + ms(60), threshold, None), Stall::None);
    assert_eq!(
        s.check(t0 + ms(61), threshold, None),
        Stall::Started(ms(61))
    );
    assert_eq!(s.stall_at(t0 + ms(70), threshold, None), Some(t0 + ms(70)));

    let s = stats(true);
    assert_eq!(s.stall_at(t0 + ms(5), threshold, None), Some(t0 + ms(60)));
    assert_eq!(
        s.stall_at(t0 + ms(100), threshold, None),
        Some(t0 + ms(120))
    );
    // the turn started at 90 ms
    let turn = |now: u64| Some(ms(now - 90));
    assert_eq!(
        s.stall_at(t0 + ms(100), threshold, turn(100)),
        Some(t0 + ms(110))
    );
    assert_eq!(s.check(t0 + ms(110), threshold, turn(110)), Stall::None);
    assert_eq!(
        s.check(t0 + ms(111), threshold, turn(111)),
        Stall::Started(ms(21))
    );

    let s = stats(true);
    // the turn started at 10 ms
    let turn = |now: u64| Some(ms(now - 10));
    assert_eq!(
        s.stall_at(t0 + ms(30), threshold, turn(30)),
        Some(t0 + ms(60))
    );
    assert_eq!(s.check(t0 + ms(60), threshold, turn(60)), Stall::None);
    assert_eq!(
        s.check(t0 + ms(61), threshold, turn(61)),
        Stall::Started(ms(51))
    );
}

/// STATEMENT: Only an idle arbiter with deferred sampling, a due sample and no stall is dormant.
///
/// HOW:
/// - Check an idle arbiter past its deadline is not dormant without deferred sampling.
/// - With deferred sampling, check it is dormant at the deadline but not before it
///   or while a turn runs.
/// - Mark it stalled and check it is not dormant.
#[test]
fn dormant_arbiter() {
    let ms = Duration::from_millis;
    let stats = LatencyStats::default();
    let t0 = Instant::now();
    stats.base.set(t0).unwrap();
    stats.interval.store(as_nanos(ms(10)), Ordering::Relaxed);
    stats.expect(t0 + ms(10));
    assert!(!stats.is_dormant(t0 + ms(20), None));

    stats.deferred.store(true, Ordering::Relaxed);
    assert!(!stats.is_dormant(t0 + ms(5), None));
    assert!(stats.is_dormant(t0 + ms(10), None));
    assert!(!stats.is_dormant(t0 + ms(20), Some(ms(1))));

    stats.stalled.store(true, Ordering::Relaxed);
    assert!(!stats.is_dormant(t0 + ms(20), None));
}
