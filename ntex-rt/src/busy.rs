use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const BUSY: u64 = 1 << 63;

/// A turn that runs at least this long, in nanoseconds, is counted by
/// `long_turns`.
const LONG_TURN_NANOS: u64 = 100_000_000;

/// Busy time of a runtime, written by the runtime thread and read from any
/// thread, including while the runtime is busy.
#[derive(Debug)]
pub(crate) struct BusyClock {
    origin: Instant,
    // nanoseconds since `origin`. While idle, the total busy time. While busy,
    // `BUSY` and the start of the busy period minus the busy time before it,
    // so the busy time is the current time minus this value.
    state: AtomicU64,
    // start of the current turn in nanoseconds since `origin` plus one, zero
    // while idle
    turn: AtomicU64,
    // number of ended turns that ran for at least `LONG_TURN_NANOS`
    long: AtomicU64,
    // the largest busy time returned by `busy_duration`. A reader that reads
    // the clock after the runtime thread read it for `stop`, but before the
    // state is published, returns more than the published total.
    reported: AtomicU64,
}

impl BusyClock {
    pub(crate) fn new() -> Self {
        Self {
            origin: Instant::now(),
            state: AtomicU64::new(0),
            turn: AtomicU64::new(0),
            long: AtomicU64::new(0),
            reported: AtomicU64::new(0),
        }
    }

    fn nanos(&self, t: Instant) -> u64 {
        u64::try_from(t.saturating_duration_since(self.origin).as_nanos())
            .unwrap_or(u64::MAX)
            .min(BUSY - 1)
    }

    /// Starts a busy period and its first turn, if the runtime is idle.
    ///
    /// Must be called on the runtime thread.
    pub(crate) fn start(&self, now: Instant) {
        let state = self.state.load(Ordering::Relaxed);
        if state & BUSY == 0 {
            let now = self.nanos(now);
            self.turn.store(now + 1, Ordering::Relaxed);
            self.state
                .store(BUSY | now.saturating_sub(state), Ordering::Release);
        }
    }

    /// Starts the next turn of the busy period.
    ///
    /// Must be called on the runtime thread.
    pub(crate) fn next_turn(&self, now: Instant) {
        let now = self.nanos(now);
        // a reader that sees the next turn sees the previous one counted
        let order = if self.end_turn(now) {
            Ordering::Release
        } else {
            Ordering::Relaxed
        };
        self.turn.store(now + 1, order);
    }

    /// Counts the current turn if it ran for at least `LONG_TURN_NANOS` until
    /// `now`, returns `true` if it did.
    fn end_turn(&self, now: u64) -> bool {
        let turn = self.turn.load(Ordering::Relaxed);
        let long = turn != 0 && now.saturating_sub(turn - 1) >= LONG_TURN_NANOS;
        if long {
            // written only by the runtime thread
            let count = self.long.load(Ordering::Relaxed);
            self.long.store(count + 1, Ordering::Relaxed);
        }
        long
    }

    /// Ends the busy period, if the runtime is busy.
    ///
    /// Must be called on the runtime thread.
    pub(crate) fn stop(&self, now: Instant) {
        let state = self.state.load(Ordering::Relaxed);
        if state & BUSY != 0 {
            let now = self.nanos(now);
            self.end_turn(now);
            // a reader that sees the idle runtime sees the writes of its turns
            self.turn.store(0, Ordering::Release);
            self.state
                .store(now.saturating_sub(state & !BUSY), Ordering::Release);
        }
    }

    /// Returns the total busy time, including the current busy period.
    ///
    /// The result never decreases between calls, from any thread.
    pub(crate) fn busy_duration(&self) -> Duration {
        let busy = self.read();
        let prev = self.reported.fetch_max(busy, Ordering::Relaxed);
        Duration::from_nanos(busy.max(prev))
    }

    fn read(&self) -> u64 {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & BUSY == 0 {
                return state;
            }
            // the busy period is still running if the state did not change
            // while the clock was read
            let now = self.nanos(Instant::now());
            let next = self.state.load(Ordering::Acquire);
            if next == state {
                return now.saturating_sub(state & !BUSY);
            }
            state = next;
        }
    }

    /// Returns the number of ended turns that ran for at least `LONG_TURN_NANOS`.
    ///
    /// A reader that sees a turn start, see `current_turn`, sees the
    /// previous turn counted.
    pub(crate) fn long_turns(&self) -> u64 {
        self.long.load(Ordering::Acquire)
    }

    /// Returns how long the current turn has been running, `None` while idle.
    pub(crate) fn current_turn(&self) -> Option<Duration> {
        let turn = self.turn.load(Ordering::Acquire);
        (turn != 0)
            .then(|| Duration::from_nanos(self.nanos(Instant::now()).saturating_sub(turn - 1)))
    }
}

#[cfg(test)]
#[path = "../tests/unit/busy.rs"]
mod tests;
