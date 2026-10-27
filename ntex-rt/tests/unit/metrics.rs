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
    assert_eq!(stats.check(t0 + ms(100), threshold), Stall::None);

    stats.base.set(t0).unwrap();
    stats.interval.store(as_nanos(ms(10)), Ordering::Relaxed);
    assert_eq!(stats.check(t0 + ms(30), threshold), Stall::None);
    assert_eq!(stats.check(t0 + ms(31), threshold), Stall::Started(ms(31)));
    assert_eq!(stats.check(t0 + ms(50), threshold), Stall::None);
    let m = stats.metrics().unwrap();
    assert_eq!((m.stalls, m.stall_duration, m.stalled), (1, ms(0), true));

    stats.record(ms(90), t0 + ms(100));
    assert_eq!(
        stats.check(t0 + ms(101), threshold),
        Stall::Recovered(ms(90))
    );
    assert_eq!(stats.check(t0 + ms(120), threshold), Stall::None);
    let m = stats.metrics().unwrap();
    assert_eq!((m.stalls, m.stall_duration, m.stalled), (1, ms(90), false));

    assert_eq!(stats.check(t0 + ms(131), threshold), Stall::Started(ms(31)));
    assert_eq!(stats.metrics().unwrap().stalls, 2);
}
