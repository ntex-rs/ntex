use std::hash::{BuildHasher, RandomState};
use std::time::{Duration, Instant};

use ntex_rt::Id;
use ntex_util::HashMap;

use crate::Worker;

/// How the server distributes items between workers.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LoadBalancing {
    /// Items are sent to the workers in turn.
    ///
    /// Workers whose runtime is blocked by a task are skipped while another
    /// worker is available.
    RoundRobin,
    /// Each item is sent to the less loaded of two randomly chosen workers.
    ///
    /// A worker's load is its runtime utilization, averaged over the last
    /// ~250 ms. Workers whose utilization differs by less than 10% are compared
    /// by their open connections plus queued items. Workers whose runtime
    /// is blocked by a task are skipped while another worker is available.
    LeastLoaded,
    /// Each connection is sent to the worker placed on the CPU that receives
    /// its network packets, unless that worker is busier than other workers.
    ///
    /// The worker on the incoming CPU, or on an SMT sibling of it if no worker
    /// is placed on that CPU, is used if its utilization exceeds the
    /// [`LeastLoaded`](Self::LeastLoaded) choice by at most 20% and its load by
    /// at most half plus 16. Otherwise, and for items without a known
    /// incoming CPU, it works like [`LeastLoaded`](Self::LeastLoaded).
    ///
    /// The incoming CPU is the receive-side scaling (RSS) CPU of the
    /// connection on Windows, known for connections over network adapters
    /// with RSS, and the CPU that processed the connection's last packet
    /// (`SO_INCOMING_CPU`) on Linux and Android. Requires worker affinity, see
    /// [`Affinity`](crate::Affinity).
    ///
    /// This is the default.
    #[default]
    IncomingCpu,
}

/// Minimum interval between worker load samples.
const REFRESH: Duration = Duration::from_millis(10);
/// Time constant of the utilization average.
const TAU: Duration = Duration::from_millis(250);
/// A runtime turn longer than this marks the worker as blocked.
const BLOCKED: Duration = Duration::from_millis(100);
/// Utilization difference below which workers are compared by load.
const UTIL_SLACK: f64 = 0.1;
/// Utilization by which the worker on the incoming CPU may exceed the
/// least loaded choice.
const LOCAL_UTIL_SLACK: f64 = 0.2;
/// Load by which the worker on the incoming CPU may exceed the least loaded
/// choice, on top of half of that choice's load.
const LOCAL_LOAD_SLACK: usize = 16;

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
    /// Indexes of the workers placed on each CPU, or on an SMT sibling of a
    /// CPU without its own worker.
    local: HashMap<usize, Vec<usize>>,
    /// `local` must be rebuilt.
    local_stale: bool,
    /// Load state of unavailable workers by name, with their arbiter.
    parked: HashMap<String, (Id, State)>,
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
            local: HashMap::default(),
            local_stale: true,
            parked: HashMap::default(),
        }
    }

    #[cfg(test)]
    /// A worker is inserted at `idx`.
    pub(crate) fn insert(&mut self, idx: usize) {
        self.insert_state(idx, State::default());
    }

    /// A worker that became available again is inserted at `idx`.
    ///
    /// Its load state is kept from when it became unavailable, unless the
    /// worker was restarted on a new arbiter since then.
    pub(crate) fn unpark(&mut self, idx: usize, name: &str, arbiter: Id) {
        let state = match self.parked.remove(name) {
            Some((id, state)) if id == arbiter => state,
            _ => State::default(),
        };
        self.insert_state(idx, state);
    }

    fn insert_state(&mut self, idx: usize, state: State) {
        self.states.insert(idx, state);
        self.refreshed = None;
        self.local_stale = true;
    }

    /// The worker at `idx` is replaced.
    pub(crate) fn reset(&mut self, idx: usize) {
        self.states[idx] = State::default();
        self.refreshed = None;
        self.local_stale = true;
    }

    /// The worker at `idx` is removed.
    pub(crate) fn remove(&mut self, idx: usize) {
        self.states.remove(idx);
        self.refreshed = None;
        self.local_stale = true;
    }

    /// The worker at `idx` became unavailable and is removed, its load state
    /// is kept for [`unpark`](Self::unpark).
    pub(crate) fn park(&mut self, idx: usize, name: &str, arbiter: Id) {
        let state = self.states.remove(idx);
        self.parked.insert(name.to_owned(), (arbiter, state));
        self.refreshed = None;
        self.local_stale = true;
    }

    /// Returns `true` if [`select`](Self::select) uses the incoming CPU of
    /// items, i.e. the mode is [`LoadBalancing::IncomingCpu`] and some
    /// workers are placed on CPUs.
    pub(crate) fn uses_incoming_cpu<T>(&mut self, workers: &[Worker<T>]) -> bool {
        if self.mode != LoadBalancing::IncomingCpu {
            return false;
        }
        if self.local_stale {
            self.local_stale = false;
            self.map_cpus(workers.iter().map(Worker::cpus));
        }
        !self.local.is_empty()
    }

    /// Returns the index of the worker for the next item, and whether it was
    /// chosen for the incoming CPU of the item if that CPU is set.
    ///
    /// `workers` must not be empty.
    pub(crate) fn select<T>(
        &mut self,
        workers: &[Worker<T>],
        cpu: Option<usize>,
    ) -> (usize, Option<bool>) {
        match self.mode {
            LoadBalancing::RoundRobin => (self.round_robin(workers.len()), None),
            LoadBalancing::LeastLoaded | LoadBalancing::IncomingCpu => {
                let now = Instant::now();
                self.refresh(now, |idx| Sample::of(&workers[idx], now));
                let load = |idx: usize| workers[idx].load();
                let idx = self.least_loaded(load);
                match cpu {
                    Some(cpu) if self.uses_incoming_cpu(workers) => {
                        let local = self.local_worker(cpu, idx, load);
                        (local.unwrap_or(idx), Some(local.is_some()))
                    }
                    _ => (idx, None),
                }
            }
        }
    }

    /// Next worker in turn that is not blocked, `len` workers.
    fn round_robin(&mut self, len: usize) -> usize {
        if self.next >= len {
            self.next = len - 1;
        }
        // blocked flags are refreshed by `sample`
        (0..len)
            .map(|i| (self.next + i) % len)
            .find(|&i| !self.is_blocked(i))
            .unwrap_or(self.next)
    }

    /// Refreshes the load state of the workers.
    ///
    /// Returns `true` if some workers are blocked and others are not.
    pub(crate) fn sample<T>(&mut self, workers: &[Worker<T>]) -> bool {
        let now = Instant::now();
        self.refresh(now, |idx| Sample::of(&workers[idx], now));
        !self.ready.is_empty() && self.ready.len() < self.states.len()
    }

    /// Returns `true` if the worker at `idx` was blocked when last sampled.
    pub(crate) fn is_blocked(&self, idx: usize) -> bool {
        self.states.get(idx).is_some_and(|st| st.blocked)
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

    /// Maps CPUs to the workers placed on them, `cpus` yields the CPU of
    /// each worker followed by its SMT siblings.
    fn map_cpus<'a>(&mut self, cpus: impl Iterator<Item = &'a [usize]>) {
        let cpus: Vec<_> = cpus.collect();
        self.local.clear();
        for (idx, cpus) in cpus.iter().enumerate() {
            if let Some(&cpu) = cpus.first() {
                self.local.entry(cpu).or_default().push(idx);
            }
        }
        let mut siblings = HashMap::<usize, Vec<usize>>::default();
        for (idx, cpus) in cpus.iter().enumerate() {
            for cpu in cpus.iter().skip(1) {
                if !self.local.contains_key(cpu) {
                    siblings.entry(*cpu).or_default().push(idx);
                }
            }
        }
        self.local.extend(siblings);
    }

    /// Returns the least loaded worker for the incoming `cpu`, unless it is
    /// busier than `other`, the least loaded choice.
    fn local_worker(
        &self,
        cpu: usize,
        other: usize,
        load: impl Fn(usize) -> usize,
    ) -> Option<usize> {
        let util = |idx: usize| self.states[idx].util;
        let best = self
            .local
            .get(&cpu)?
            .iter()
            .copied()
            .filter(|&idx| !self.states[idx].blocked)
            .min_by(|&a, &b| {
                util(a)
                    .total_cmp(&util(b))
                    .then_with(|| load(a).cmp(&load(b)))
            })?;
        let max_load = load(other) + load(other) / 2 + LOCAL_LOAD_SLACK;
        (best == other || (util(best) <= util(other) + LOCAL_UTIL_SLACK && load(best) <= max_load))
            .then_some(best)
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
    fn round_robin_skips_blocked_workers() {
        let t = Instant::now();
        let mut b = balancer(3);
        b.refresh(t, |i| sample(0, i == 1));
        let mut seen = Vec::new();
        for _ in 0..4 {
            let idx = b.round_robin(3);
            b.sent(idx, 3);
            seen.push(idx);
        }
        assert_eq!(seen, [0, 2, 0, 2]);

        // all workers are blocked
        b.refresh(t + REFRESH, |_| sample(0, true));
        assert_eq!(b.round_robin(3), 0);
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

    #[ntex::test]
    async fn keeps_state_of_unavailable_workers() {
        let (a1, a2) = (ntex_rt::Arbiter::new(), ntex_rt::Arbiter::new());
        let t = Instant::now();
        let mut b = balancer(2);
        b.refresh(t, |_| sample(0, false));
        b.refresh(t + MS * 100, |i| {
            sample(if i == 0 { 100 } else { 10 }, false)
        });
        let util = b.states[0].util;
        assert!(util > b.states[1].util + UTIL_SLACK);

        // the busy worker becomes unavailable and available again
        b.park(0, "w0", a1.id());
        b.unpark(0, "w0", a1.id());
        assert!((b.states[0].util - util).abs() < f64::EPSILON);
        for _ in 0..100 {
            assert_eq!(b.least_loaded(|i| [1, 100][i]), 1);
        }

        // a restarted worker starts over
        b.park(0, "w0", a1.id());
        b.unpark(0, "w0", a2.id());
        assert!(b.states[0].util.abs() < f64::EPSILON);
        assert!(b.parked.is_empty());
        a1.stop();
        a2.stop();
    }

    #[test]
    fn map_cpus() {
        let mut b = balancer(5);
        // workers 0 and 1 on the two CPUs of core (0, 1), workers 2 and 3
        // on CPU 2 and 4 of cores (2, 3) and (4, 5), worker 4 not placed
        b.map_cpus([&[0, 1][..], &[1, 0], &[2, 3], &[4, 5], &[]].into_iter());
        let mut local: Vec<_> = b.local.iter().map(|(c, w)| (*c, w.clone())).collect();
        local.sort();
        assert_eq!(
            local,
            [
                (0, vec![0]),
                (1, vec![1]),
                (2, vec![2]),
                (3, vec![2]),
                (4, vec![3]),
                (5, vec![3])
            ]
        );

        // several workers on one CPU
        b.map_cpus([&[0][..], &[0]].into_iter());
        assert_eq!(b.local.get(&0), Some(&vec![0, 1]));
        assert_eq!(b.local.len(), 1);
    }

    #[test]
    fn local_worker() {
        let t = Instant::now();
        let mut b = balancer(4);
        b.map_cpus([&[0][..], &[1], &[2], &[2]].into_iter());
        b.refresh(t, |_| sample(0, false));
        b.refresh(t + MS * 100, |i| sample([10, 0, 100, 90][i], false));
        let load = |_| 10;

        // the worker on the incoming CPU is preferred
        assert_eq!(b.local_worker(0, 1, load), Some(0));
        // the least utilized of the workers on the CPU
        assert_eq!(b.local_worker(2, 2, load), Some(3));
        // no worker on the CPU
        assert_eq!(b.local_worker(7, 1, load), None);
        // utilization exceeds the other choice by more than the slack
        assert!(b.states[3].util > b.states[1].util + LOCAL_UTIL_SLACK);
        assert_eq!(b.local_worker(2, 1, load), None);
        b.map_cpus([&[0][..], &[1], &[2], &[3]].into_iter());
        assert_eq!(b.local_worker(2, 1, load), None);
        assert_eq!(b.local_worker(2, 2, load), Some(2));

        // load exceeds the other choice by more than half plus the slack
        let load = |i: usize| [100, 10, 0, 0][i];
        assert_eq!(b.local_worker(0, 1, load), None);
        let load = |i: usize| [100, 56, 0, 0][i];
        assert_eq!(b.local_worker(0, 1, load), Some(0));

        // blocked worker
        b.refresh(t + MS * 200, |i| sample([10, 0, 100, 90][i], i == 0));
        assert_eq!(b.local_worker(0, 1, |_| 0), None);
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
