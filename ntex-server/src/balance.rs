use std::hash::{BuildHasher, RandomState};
use std::time::{Duration, Instant};

use crate::Worker;

/// How the server distributes items between workers.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LoadBalancing {
    /// Items are sent to the workers in turn.
    RoundRobin,
    /// Each item is sent to the less loaded of two randomly chosen workers.
    ///
    /// A worker's load is its runtime utilization, averaged over the last
    /// ~250 ms. Workers whose utilization differs by less than 10% are compared
    /// by their open connections plus queued items. Workers whose runtime
    /// is blocked by a task are skipped while another worker is available.
    ///
    /// This is the default.
    #[default]
    LeastLoaded,
}

/// Minimum interval between worker load samples.
const REFRESH: Duration = Duration::from_millis(10);
/// Time constant of the utilization average.
const TAU: Duration = Duration::from_millis(250);
/// A runtime turn longer than this marks the worker as blocked.
const BLOCKED: Duration = Duration::from_millis(100);
/// Utilization difference below which workers are compared by load.
const UTIL_SLACK: f64 = 0.1;

#[derive(Copy, Clone, Debug, Default)]
/// Worker runtime sample.
pub(crate) struct Sample {
    /// Total time the runtime has been busy.
    busy: Option<Duration>,
    /// The runtime is blocked by a task.
    blocked: bool,
}

impl Sample {
    fn of<T>(wrk: &Worker<T>, now: Instant) -> Sample {
        let arb = wrk.arbiter();
        let hnd = arb.handle();
        let blocked = hnd.current_turn().is_some_and(|t| t >= BLOCKED)
            || arb.latency().is_some_and(|l| {
                l.stalled
                    || l.last_sample
                        .is_some_and(|t| now.saturating_duration_since(t) >= l.interval + BLOCKED)
            });
        Sample {
            busy: hnd.busy_duration(),
            blocked,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    busy: Option<Duration>,
    at: Option<Instant>,
    util: f64,
    blocked: bool,
}

impl State {
    fn update(&mut self, sample: Sample, now: Instant) {
        if let (Some(busy), Some(prev), Some(at)) = (sample.busy, self.busy, self.at) {
            let dt = now.saturating_duration_since(at);
            if !dt.is_zero() {
                let used = busy.saturating_sub(prev).as_secs_f64() / dt.as_secs_f64();
                let alpha = dt.as_secs_f64() / (TAU + dt).as_secs_f64();
                self.util += alpha * (used.min(1.0) - self.util);
            }
        }
        self.busy = sample.busy;
        self.at = Some(now);
        self.blocked = sample.blocked;
    }
}

#[derive(Debug)]
/// Selects the worker for the next item.
///
/// Keeps a load state per worker, in the order of the manager's worker list.
pub(crate) struct Balancer {
    mode: LoadBalancing,
    next: usize,
    states: Vec<State>,
    /// Indexes of the workers that are not blocked.
    ready: Vec<usize>,
    refreshed: Option<Instant>,
    rng: u64,
}

impl Balancer {
    pub(crate) fn new(mode: LoadBalancing) -> Self {
        Self {
            mode,
            next: 0,
            states: Vec::new(),
            ready: Vec::new(),
            refreshed: None,
            rng: RandomState::new().hash_one(0u64) | 1,
        }
    }

    /// A worker is inserted at `idx`.
    pub(crate) fn insert(&mut self, idx: usize) {
        self.states.insert(idx, State::default());
        self.refreshed = None;
    }

    /// The worker at `idx` is replaced.
    pub(crate) fn reset(&mut self, idx: usize) {
        self.states[idx] = State::default();
        self.refreshed = None;
    }

    /// The worker at `idx` is removed.
    pub(crate) fn remove(&mut self, idx: usize) {
        self.states.remove(idx);
        self.refreshed = None;
    }

    /// Returns the index of the worker for the next item.
    ///
    /// `workers` must not be empty.
    pub(crate) fn select<T>(&mut self, workers: &[Worker<T>]) -> usize {
        match self.mode {
            LoadBalancing::RoundRobin => {
                if self.next >= workers.len() {
                    self.next = workers.len() - 1;
                }
                self.next
            }
            LoadBalancing::LeastLoaded => {
                let now = Instant::now();
                self.refresh(now, |idx| Sample::of(&workers[idx], now));
                self.least_loaded(|idx| workers[idx].load())
            }
        }
    }

    /// The item was sent to the worker at `idx`.
    pub(crate) fn sent(&mut self, idx: usize, len: usize) {
        self.next = (idx + 1) % len;
    }

    fn refresh(&mut self, now: Instant, sample: impl Fn(usize) -> Sample) {
        if self
            .refreshed
            .is_some_and(|t| now.saturating_duration_since(t) < REFRESH)
        {
            return;
        }
        self.refreshed = Some(now);
        self.ready.clear();
        for (idx, st) in self.states.iter_mut().enumerate() {
            st.update(sample(idx), now);
            if !st.blocked {
                self.ready.push(idx);
            }
        }
    }

    /// Power of two choices among the workers that are not blocked.
    fn least_loaded(&mut self, load: impl Fn(usize) -> usize) -> usize {
        let all = self.ready.is_empty();
        let len = if all { self.states.len() } else { self.ready.len() };
        if len < 2 {
            return if all { 0 } else { self.ready[0] };
        }

        let a = self.random(len);
        let mut b = self.random(len - 1);
        if b >= a {
            b += 1;
        }
        let (a, b) = if all {
            (a, b)
        } else {
            (self.ready[a], self.ready[b])
        };

        let (ua, ub) = (self.states[a].util, self.states[b].util);
        if (ua - ub).abs() > UTIL_SLACK {
            if ua < ub { a } else { b }
        } else if load(b) < load(a) {
            b
        } else {
            a
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    /// Returns a random number in `0..n`.
    fn random(&mut self, n: usize) -> usize {
        // xorshift64
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x % n as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn balancer(n: usize) -> Balancer {
        let mut b = Balancer::new(LoadBalancing::LeastLoaded);
        for i in 0..n {
            b.insert(i);
        }
        b
    }

    fn sample(busy_ms: u32, blocked: bool) -> Sample {
        Sample {
            busy: Some(MS * busy_ms),
            blocked,
        }
    }

    #[test]
    fn utilization_average() {
        let t = Instant::now();
        let mut st = State::default();
        st.update(sample(0, false), t);
        assert!(st.util.abs() < f64::EPSILON);

        // fully busy for one time constant
        st.update(sample(250, false), t + MS * 250);
        assert!((st.util - 0.5).abs() < 1e-9, "{}", st.util);

        // idle
        st.update(sample(250, false), t + MS * 500);
        assert!((st.util - 0.25).abs() < 1e-9, "{}", st.util);

        // busy time is not available, or there is no time between samples
        st.update(Sample::default(), t + MS * 600);
        assert!((st.util - 0.25).abs() < 1e-9, "{}", st.util);
        st.update(sample(0, false), t + MS * 700);
        st.update(sample(0, false), t + MS * 700);
        assert!((st.util - 0.25).abs() < 1e-9, "{}", st.util);
    }

    #[test]
    fn refresh_interval() {
        let t = Instant::now();
        let mut b = balancer(2);
        b.refresh(t, |_| sample(0, false));
        assert_eq!(b.ready, [0, 1]);

        // samples are taken at most every `REFRESH`
        b.refresh(t + MS, |_| sample(0, true));
        assert_eq!(b.ready, [0, 1]);
        b.refresh(t + REFRESH, |i| sample(0, i == 0));
        assert_eq!(b.ready, [1]);

        // worker list changes force a refresh
        b.insert(2);
        b.refresh(t + REFRESH, |_| sample(0, false));
        assert_eq!(b.ready, [0, 1, 2]);
    }

    #[test]
    fn skips_blocked_workers() {
        let t = Instant::now();
        let mut b = balancer(3);
        b.refresh(t, |i| sample(0, i != 2));
        for _ in 0..100 {
            assert_eq!(b.least_loaded(|_| 0), 2);
        }

        // all workers are blocked
        b.refresh(t + REFRESH, |_| sample(0, true));
        let mut seen = [false; 3];
        for _ in 0..100 {
            seen[b.least_loaded(|_| 0)] = true;
        }
        assert_eq!(seen, [true; 3]);
    }

    #[test]
    fn prefers_less_utilized_workers() {
        let t = Instant::now();
        let mut b = balancer(2);
        b.refresh(t, |_| sample(0, false));
        b.refresh(t + MS * 100, |i| {
            sample(if i == 0 { 100 } else { 10 }, false)
        });
        assert!(b.states[0].util > b.states[1].util + UTIL_SLACK);
        for _ in 0..100 {
            // utilization wins over load
            assert_eq!(b.least_loaded(|i| if i == 0 { 0 } else { 100 }), 1);
        }
    }

    #[test]
    fn compares_load_of_similar_workers() {
        let t = Instant::now();
        let mut b = balancer(4);
        b.refresh(t, |_| sample(0, false));
        b.refresh(t + MS * 100, |i| sample([10, 11, 12, 13][i], false));
        let mut seen = [0; 4];
        for _ in 0..1000 {
            seen[b.least_loaded(|i| [5, 1, 2, 3][i])] += 1;
        }
        // the most loaded worker never wins a comparison
        assert_eq!(seen[0], 0);
        assert!(
            seen[1] > seen[2] && seen[2] > seen[3] && seen[3] > 0,
            "{seen:?}"
        );
    }

    #[test]
    fn single_worker() {
        let mut b = balancer(1);
        b.refresh(Instant::now(), |_| sample(0, true));
        assert_eq!(b.least_loaded(|_| 0), 0);
    }

    #[test]
    fn random_range() {
        let mut b = balancer(0);
        let mut seen = [false; 5];
        for _ in 0..1000 {
            seen[b.random(5)] = true;
        }
        assert_eq!(seen, [true; 5]);
    }
}
