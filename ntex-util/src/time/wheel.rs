//! Hierarchical timer wheel backing ntex timers.
//!
//! The design follows the Linux kernel timer wheel (`kernel/time/timer.c`).
//!
//! # Clock
//!
//! The wheel counts time in *units* of 16 milliseconds (`1 << UNITS`). The
//! wheel clock `elapsed` is the unit of the last processed expiry, and
//! `elapsed_time` is the [`Instant`] that corresponds to it. Timer deadlines
//! are converted to units relative to this pair.
//!
//! # Levels
//!
//! The wheel has `LVL_DEPTH` (8) levels of `LVL_SIZE` (64) buckets. Each level
//! is 8 times coarser than the previous one, a timer is placed on the first
//! level that can represent its delay:
//!
//! | Level | Granularity | Range            |
//! |-------|-------------|------------------|
//! | 0     | 16 ms       | 0 .. ~1 s        |
//! | 1     | 128 ms      | ~1 s .. ~8 s     |
//! | 2     | ~1 s        | ~8 s .. ~64 s    |
//! | 3     | ~8 s        | ~64 s .. ~8.6 m  |
//! | 4     | ~65 s       | ~8.6 m .. ~69 m  |
//! | 5     | ~8.7 m      | ~69 m .. ~9.2 h  |
//! | 6     | ~70 m       | ~9.2 h .. ~3 d   |
//! | 7     | ~9.3 h      | ~3 d .. ~24.5 d  |
//!
//! Longer delays are clamped to the capacity of the wheel. The expiry is
//! rounded up to the granularity of the level and the delay is measured from
//! [`Instant::now()`], so a timer never fires early but may fire up to one
//! granularity late. Timers are not cascaded to finer
//! levels, which keeps insertion and removal `O(1)`.
//!
//! A bitmap per level tracks occupied buckets, the next expiry is found by
//! scanning the bitmaps instead of the buckets.
//!
//! # Drivers
//!
//! Two tasks are spawned lazily on the current thread:
//!
//! * [`TimerDriver`] sleeps until the next occupied bucket expires and wakes
//!   the timers stored in it. The clock advances to the scheduled time of the
//!   bucket, not to the time the driver woke up, so a late wakeup does not
//!   delay later timers. Buckets that are overdue are processed at once.
//! * [`LowresTimerDriver`] invalidates the cached [`now()`] and
//!   [`system_time()`] values every 150 milliseconds.
//!
//! Dropping the timer driver, i.e. when the runtime stops, stops the wheel and
//! marks all timers as elapsed. Dropping the lowres driver invalidates the
//! cached time.
//!
//! A runtime does not always drop its pending tasks when it stops, the timer
//! is also reset once the arbiter storage of the stopped system is cleared,
//! so that the next runtime on the thread starts its own drivers. Drivers of
//! an older generation exit without touching the state.
use std::num::NonZeroUsize;
use std::time::{Duration, Instant, SystemTime};
use std::{cell::Cell, cmp, future::Future, pin::Pin, rc::Rc, task, task::Poll};

use futures_timer::Delay;
use slab::Slab;

use crate::task::LocalWaker;

/// Resolution of the wheel clock, a unit is `1 << UNITS` milliseconds.
const UNITS: u64 = 4;

/// Each level is `LVL_CLK_DIV` times coarser than the previous one.
const LVL_CLK_SHIFT: u64 = 3;
const LVL_CLK_DIV: u64 = 1 << LVL_CLK_SHIFT;
const LVL_CLK_MASK: u64 = LVL_CLK_DIV - 1;

/// Number of buckets per level.
const LVL_BITS: u64 = 6;
const LVL_SIZE: u64 = 1 << LVL_BITS;
const LVL_MASK: u64 = LVL_SIZE - 1;

/// Number of levels.
const LVL_DEPTH: u64 = 8;

/// Total number of buckets.
const WHEEL_SIZE: usize = (LVL_SIZE * LVL_DEPTH) as usize;

/// Delays at or above the cutoff are clamped to `WHEEL_TIMEOUT_MAX`.
const WHEEL_TIMEOUT_CUTOFF: u64 = lvl_start(LVL_DEPTH);
const WHEEL_TIMEOUT_MAX: u64 = WHEEL_TIMEOUT_CUTOFF - lvl_gran(LVL_DEPTH - 1);

/// Refresh interval of the cached time.
const LOWRES_RESOLUTION: Duration = Duration::from_millis(150);

/// Shift of the level clock relative to the wheel clock.
const fn lvl_shift(lvl: u64) -> u64 {
    lvl * LVL_CLK_SHIFT
}

/// Granularity of a level in units.
const fn lvl_gran(lvl: u64) -> u64 {
    1 << lvl_shift(lvl)
}

/// Smallest delay in units that is stored on level `lvl`, `lvl` must be at
/// least 1.
const fn lvl_start(lvl: u64) -> u64 {
    (LVL_SIZE - 1) << ((lvl - 1) * LVL_CLK_SHIFT)
}

const fn to_units(millis: u64) -> u64 {
    millis >> UNITS
}

const fn to_millis(units: u64) -> u64 {
    units << UNITS
}

const fn as_millis(dur: Duration) -> u64 {
    dur.as_secs() * 1_000 + (dur.subsec_millis() as u64)
}

/// Returns a cached approximation of the current instant.
///
/// The cached value is refreshed at roughly 150 millisecond intervals.
#[inline]
pub fn now() -> Instant {
    TIMER.with(Timer::now)
}

/// Returns a cached approximation of the current system time.
///
/// The cached value is refreshed at roughly 150 millisecond intervals.
#[inline]
pub fn system_time() -> SystemTime {
    TIMER.with(Timer::system_time)
}

#[derive(Debug)]
/// Handle to a timer registered with ntex's local timer wheel.
///
/// Dropping the handle cancels the timer. A handle may be reset and reused
/// after it has elapsed.
pub struct TimerHandle(NonZeroUsize);

impl TimerHandle {
    /// Registers a timer that elapses after `millis`.
    pub fn new(millis: u64) -> Self {
        TIMER.with(|t| t.add_timer(millis))
    }

    /// Restarts the timer with a new delay in milliseconds.
    pub fn reset(&self, millis: u64) {
        TIMER.with(|t| t.update_timer(self.0.get(), millis));
    }

    /// Completes the timer immediately and wakes its registered task.
    pub fn elapse(&self) {
        TIMER.with(|t| t.remove_timer(self.0.get()));
    }

    /// Returns `true` if this timer has elapsed.
    pub fn is_elapsed(&self) -> bool {
        TIMER.with(|t| t.with_wheel(|w| w.timers[self.0.get()].bucket.is_none()))
    }

    /// Polls until this timer has elapsed.
    pub fn poll_elapsed(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        TIMER.with(|t| {
            t.with_wheel(|w| {
                let entry = &w.timers[self.0.get()];
                if entry.bucket.is_none() {
                    Poll::Ready(())
                } else {
                    entry.task.register(cx.waker());
                    Poll::Pending
                }
            })
        })
    }
}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        // the wheel is already destroyed if the handle is dropped by
        // another thread-local destructor
        let _ = TIMER.try_with(|t| {
            t.with_wheel(|w| {
                w.unlink(self.0.get());
                w.timers.remove(self.0.get());
            });
        });
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    struct Flags: u8 {
        /// The timer driver task is spawned.
        const DRIVER_STARTED = 0b0000_0001;
        /// The cached time is populated and its refresh sleep is armed.
        const LOWRES_TIMER   = 0b0000_1000;
        /// The lowres driver task is spawned.
        const LOWRES_DRIVER  = 0b0001_0000;
        /// A timer was scheduled, `now()` and `system_time()` cache the time.
        const RUNNING        = 0b0010_0000;
    }
}

thread_local! {
    static TIMER: Rc<Timer> = Rc::new(Timer::new());
}

/// Per-thread timer state, shared by the timer handles and the driver tasks.
struct Timer {
    /// Wheel clock in units, the expiry that was processed last.
    elapsed: Cell<u64>,
    /// Instant that corresponds to `elapsed`, the scheduled time of the last
    /// processed expiry. Set lazily when the wheel is idle.
    elapsed_time: Cell<Option<Instant>>,
    /// Expiry of the earliest occupied bucket, `u64::MAX` if the wheel is empty.
    next_expiry: Cell<u64>,
    flags: Cell<Flags>,
    /// Incremented on reset, drivers of an older generation are stale.
    generation: Cell<u64>,
    driver: LocalWaker,
    lowres_time: Cell<Option<Instant>>,
    lowres_stime: Cell<Option<SystemTime>>,
    lowres_driver: LocalWaker,
    /// Taken out of the cell for the duration of an operation.
    wheel: Cell<Option<Box<Wheel>>>,
}

/// Timer storage.
struct Wheel {
    /// Timer entries indexed by handle, slot 0 is reserved so handles are
    /// non-zero.
    timers: Slab<TimerEntry>,
    /// Handles of the timers stored in each bucket, `lvl * LVL_SIZE + offset`.
    buckets: Box<[Slab<usize>]>,
    /// Occupied buckets of each level, bit `n` is set if bucket `n` has timers.
    occupied: [u64; LVL_DEPTH as usize],
}

#[derive(Debug)]
struct TimerEntry {
    /// Bucket index, `None` once the timer has elapsed.
    bucket: Option<u16>,
    /// Key of the entry in the bucket.
    bucket_entry: usize,
    task: LocalWaker,
}

impl Timer {
    fn new() -> Self {
        let mut timers = Slab::default();
        timers.insert(TimerEntry {
            bucket: None,
            bucket_entry: 0,
            task: LocalWaker::new(),
        });

        Timer {
            elapsed: Cell::new(0),
            elapsed_time: Cell::new(None),
            next_expiry: Cell::new(u64::MAX),
            flags: Cell::new(Flags::empty()),
            generation: Cell::new(0),
            driver: LocalWaker::new(),
            lowres_time: Cell::new(None),
            lowres_stime: Cell::new(None),
            lowres_driver: LocalWaker::new(),
            wheel: Cell::new(Some(Box::new(Wheel {
                timers,
                buckets: (0..WHEEL_SIZE).map(|_| Slab::new()).collect(),
                occupied: [0; LVL_DEPTH as usize],
            }))),
        }
    }

    fn with_wheel<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Wheel) -> R,
    {
        let mut wheel = self.wheel.take().unwrap();
        let result = f(&mut wheel);
        self.wheel.set(Some(wheel));
        result
    }

    fn insert_flags(&self, flags: Flags) -> Flags {
        let mut f = self.flags.get();
        f.insert(flags);
        self.flags.set(f);
        f
    }

    /// Returns the cached instant, the cache is populated only while the wheel
    /// is running.
    fn now(self: &Rc<Self>) -> Instant {
        if let Some(cur) = self.lowres_time.get() {
            cur
        } else {
            let now = Instant::now();
            if self.flags.get().contains(Flags::RUNNING) {
                self.lowres_time.set(Some(now));
                self.refresh_lowres();
            }
            now
        }
    }

    fn system_time(self: &Rc<Self>) -> SystemTime {
        if let Some(cur) = self.lowres_stime.get() {
            cur
        } else {
            let now = SystemTime::now();
            if self.flags.get().contains(Flags::RUNNING) {
                self.lowres_stime.set(Some(now));
                self.refresh_lowres();
            }
            now
        }
    }

    /// Arms the invalidation of the cached time.
    fn refresh_lowres(self: &Rc<Self>) {
        if self.flags.get().contains(Flags::LOWRES_DRIVER) {
            self.lowres_driver.wake();
        } else {
            LowresTimerDriver::start(self);
        }
    }

    /// Instant that corresponds to the wheel clock.
    fn elapsed_time(&self) -> Instant {
        if let Some(elapsed_time) = self.elapsed_time.get() {
            elapsed_time
        } else {
            let elapsed_time = Instant::now();
            self.elapsed_time.set(Some(elapsed_time));
            elapsed_time
        }
    }

    fn add_timer(self: &Rc<Self>, millis: u64) -> TimerHandle {
        let no = if millis == 0 {
            // elapsed immediately
            self.with_wheel(|w| {
                w.timers.insert(TimerEntry {
                    bucket: None,
                    bucket_entry: 0,
                    task: LocalWaker::new(),
                })
            })
        } else {
            let (idx, expiry) = self.calc_bucket(millis);
            let no = self.with_wheel(|w| {
                let no = w.timers.insert(TimerEntry {
                    bucket: None,
                    bucket_entry: 0,
                    task: LocalWaker::new(),
                });
                w.link(no, idx);
                no
            });
            self.update_next_expiry(expiry);
            no
        };

        // slot 0 is reserved in `Timer::new()`
        TimerHandle(NonZeroUsize::new(no).unwrap())
    }

    /// Moves the timer to a new bucket, a zero delay elapses it and wakes its
    /// task.
    fn update_timer(self: &Rc<Self>, hnd: usize, millis: u64) {
        if millis == 0 {
            self.remove_timer(hnd);
        } else {
            let (idx, expiry) = self.calc_bucket(millis);
            self.with_wheel(|w| w.relink(hnd, idx));
            self.update_next_expiry(expiry);
        }
    }

    /// Elapses the timer and wakes its task.
    fn remove_timer(&self, hnd: usize) {
        self.with_wheel(|w| {
            if w.unlink(hnd) {
                w.timers[hnd].task.wake();
            }
        });
    }

    /// Returns the bucket index and the bucket expiry for a delay starting now.
    fn calc_bucket(self: &Rc<Self>, millis: u64) -> (usize, u64) {
        self.insert_flags(Flags::RUNNING);

        // The delay is measured from the wheel clock. The cached time is not
        // used, it goes stale while the thread is blocked and the timer would
        // fire early by its age
        let since = Instant::now().saturating_duration_since(self.elapsed_time());
        let delta = to_units(as_millis(since) + millis);
        self.calc_wheel_index(self.elapsed.get().wrapping_add(delta), delta)
    }

    /// Selects the level for a timer that expires at `expires`, `delta` units
    /// from now.
    fn calc_wheel_index(&self, expires: u64, delta: u64) -> (usize, u64) {
        for lvl in 0..LVL_DEPTH {
            if delta < lvl_start(lvl + 1) {
                return Self::calc_index(expires, lvl);
            }
        }
        // expire larger delays at the capacity limit of the wheel
        Self::calc_index(
            self.elapsed.get().wrapping_add(WHEEL_TIMEOUT_MAX),
            LVL_DEPTH - 1,
        )
    }

    /// Returns the bucket index and the bucket expiry on level `lvl`.
    fn calc_index(expires: u64, lvl: u64) -> (usize, u64) {
        // The timer must not fire early. Early expiry can happen because the
        // timer is armed at the edge of a tick, or because the expiry is
        // truncated to the level granularity, round up to prevent it.
        let expires = (expires + lvl_gran(lvl)) >> lvl_shift(lvl);
        (
            (lvl * LVL_SIZE + (expires & LVL_MASK)) as usize,
            expires << lvl_shift(lvl),
        )
    }

    /// Wakes the driver if the new bucket expires before the current deadline.
    fn update_next_expiry(self: &Rc<Self>, expiry: u64) {
        if expiry < self.next_expiry.get() {
            self.next_expiry.set(expiry);
            if self.flags.get().contains(Flags::DRIVER_STARTED) {
                self.driver.wake();
            } else {
                TimerDriver::start(self);
            }
        }
    }

    /// Instant at which the bucket expiring at `expiry` is due.
    fn expiry_time(&self, expiry: u64) -> Instant {
        self.elapsed_time()
            + Duration::from_millis(to_millis(expiry.saturating_sub(self.elapsed.get())))
    }

    /// Returns the expiry of the earliest occupied bucket.
    fn next_pending_bucket(&self, wheel: &Wheel) -> Option<u64> {
        let mut clk = self.elapsed.get();
        let mut next = u64::MAX;

        for lvl in 0..LVL_DEPTH {
            let lvl_clk = clk & LVL_CLK_MASK;
            let occupied = wheel.occupied[lvl as usize];

            if occupied != 0 {
                // distance to the next occupied bucket, wrapping around the level
                let pos = u64::from(
                    occupied
                        .rotate_right((clk & LVL_MASK) as u32)
                        .trailing_zeros(),
                );
                next = cmp::min(next, (clk + pos) << lvl_shift(lvl));

                // The next level is reached once the clock of this level wraps
                // to a multiple of `LVL_CLK_DIV`, an earlier bucket here cannot
                // be preceded by one of the next level.
                if pos <= ((LVL_CLK_DIV - lvl_clk) & LVL_CLK_MASK) {
                    break;
                }
            }

            // Clock of the next level. A partially elapsed tick of this level
            // is rounded up, as the next level bucket it belongs to was already
            // processed.
            clk >>= LVL_CLK_SHIFT;
            clk += u64::from(lvl_clk != 0);
        }

        if next < u64::MAX { Some(next) } else { None }
    }

    /// Removes `flags`, and `RUNNING` so the cached time is not populated and
    /// no driver is spawned after the runtime stopped.
    fn remove_flags(&self, flags: Flags) {
        let mut f = self.flags.get();
        f.remove(flags | Flags::RUNNING);
        self.flags.set(f);
    }

    /// Invalidates the cached time, called when the lowres driver is dropped.
    fn stop_lowres(&self) {
        self.remove_flags(Flags::LOWRES_DRIVER | Flags::LOWRES_TIMER);
        self.lowres_time.set(None);
        self.lowres_stime.set(None);
    }

    /// Stops the drivers of the current generation, called when the system
    /// that runs them has stopped.
    fn reset(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.stop_wheel();
        self.stop_lowres();
        self.driver.take();
        self.lowres_driver.take();
    }

    /// Marks all timers as elapsed and resets the wheel, called when the timer
    /// driver is dropped. Tasks are not woken, the runtime is stopping.
    fn stop_wheel(&self) {
        self.remove_flags(Flags::DRIVER_STARTED);

        // the wheel is in use if a driver is dropped from a timer operation
        if let Some(mut wheel) = self.wheel.take() {
            let Wheel {
                timers,
                buckets,
                occupied,
            } = &mut *wheel;
            for bucket in buckets.iter_mut() {
                for no in bucket.drain() {
                    timers[no].bucket = None;
                }
            }
            *occupied = [0; LVL_DEPTH as usize];

            self.next_expiry.set(u64::MAX);
            self.elapsed.set(0);
            self.elapsed_time.set(None);
            self.wheel.set(Some(wheel));
        }
    }
}

impl Wheel {
    /// Level and occupied bit of a bucket.
    fn bucket_bit(idx: usize) -> (usize, u64) {
        (idx / LVL_SIZE as usize, 1 << (idx % LVL_SIZE as usize))
    }

    /// Stores the timer in bucket `idx`.
    fn link(&mut self, hnd: usize, idx: usize) {
        let entry = &mut self.timers[hnd];
        entry.bucket = Some(idx as u16);
        entry.bucket_entry = self.buckets[idx].insert(hnd);

        let (lvl, bit) = Self::bucket_bit(idx);
        self.occupied[lvl] |= bit;
    }

    /// Removes the timer from its bucket and marks it as elapsed.
    ///
    /// Returns `false` if the timer has already elapsed.
    fn unlink(&mut self, hnd: usize) -> bool {
        let entry = &mut self.timers[hnd];
        if let Some(idx) = entry.bucket.take() {
            let idx = idx as usize;
            let bucket = &mut self.buckets[idx];
            bucket.remove(entry.bucket_entry);
            if bucket.is_empty() {
                let (lvl, bit) = Self::bucket_bit(idx);
                self.occupied[lvl] &= !bit;
            }
            true
        } else {
            false
        }
    }

    /// Moves the timer to bucket `idx`.
    fn relink(&mut self, hnd: usize, idx: usize) {
        if self.timers[hnd].bucket != Some(idx as u16) {
            self.unlink(hnd);
            self.link(hnd, idx);
        }
    }

    /// Wakes the timers of the buckets that expire at `clk`.
    fn execute_expired_timers(&mut self, mut clk: u64) {
        for lvl in 0..LVL_DEPTH {
            let idx = ((clk & LVL_MASK) + lvl * LVL_SIZE) as usize;
            let bucket = &mut self.buckets[idx];
            if !bucket.is_empty() {
                let (lvl, bit) = Self::bucket_bit(idx);
                self.occupied[lvl] &= !bit;
                for no in bucket.drain() {
                    let entry = &mut self.timers[no];
                    entry.bucket = None;
                    entry.task.wake();
                }
            }

            // The next level expires only when the clock is a multiple of its
            // granularity
            if (clk & LVL_CLK_MASK) != 0 {
                break;
            }
            clk >>= LVL_CLK_SHIFT;
        }
    }
}

/// Resets the timer when dropped with the arbiter storage of a stopped system.
#[derive(Default)]
struct TimerReset;

impl TimerReset {
    fn register() {
        ntex_rt::with_item::<TimerReset, _, _>(|_| ());
    }
}

impl Drop for TimerReset {
    fn drop(&mut self) {
        // the timer is already destroyed if the thread is exiting
        let _ = TIMER.try_with(|t| t.reset());
    }
}

/// Task that sleeps until the next bucket expires and wakes its timers.
struct TimerDriver {
    timer: Rc<Timer>,
    generation: u64,
    sleep: Delay,
    /// Deadline the sleep is armed for.
    armed: Option<Instant>,
}

impl TimerDriver {
    fn start(timer: &Rc<Timer>) {
        timer.insert_flags(Flags::DRIVER_STARTED);
        TimerReset::register();

        let deadline = timer.expiry_time(timer.next_expiry.get());
        crate::spawn(TimerDriver {
            timer: timer.clone(),
            generation: timer.generation.get(),
            sleep: Delay::new(deadline.saturating_duration_since(Instant::now())),
            armed: Some(deadline),
        });

        // start lowres driver
        timer.refresh_lowres();
    }
}

impl Drop for TimerDriver {
    fn drop(&mut self) {
        if self.timer.generation.get() == self.generation {
            self.timer.stop_wheel();
        }
    }
}

impl Future for TimerDriver {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let timer = &this.timer;
        if timer.generation.get() != this.generation {
            return Poll::Ready(());
        }
        timer.driver.register(cx.waker());

        let now = Instant::now();
        timer.lowres_time.set(Some(now));

        loop {
            let expiry = timer.next_expiry.get();
            if expiry == u64::MAX {
                // the wheel is empty, a new timer wakes the driver
                return Poll::Pending;
            }

            let deadline = timer.expiry_time(expiry);
            if deadline > now {
                if this.armed != Some(deadline) {
                    this.armed = Some(deadline);
                    this.sleep.reset(deadline.saturating_duration_since(now));
                }
                if Pin::new(&mut this.sleep).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                if deadline > now {
                    // the sleep fired before the deadline, re-arm it
                    this.armed = None;
                    continue;
                }
            }

            // Advance the clock to the scheduled time of the bucket, a late
            // wakeup must not shift the timers that expire later
            timer.elapsed.set(expiry);
            timer.elapsed_time.set(Some(deadline));

            let next = timer.with_wheel(|w| {
                w.execute_expired_timers(expiry);
                timer.next_pending_bucket(w)
            });

            if let Some(next) = next {
                timer.next_expiry.set(next);
            } else {
                timer.next_expiry.set(u64::MAX);
                timer.elapsed_time.set(None);
            }
        }
    }
}

/// Task that invalidates the cached time every `LOWRES_RESOLUTION`.
struct LowresTimerDriver {
    timer: Rc<Timer>,
    generation: u64,
    sleep: Delay,
}

impl LowresTimerDriver {
    fn start(timer: &Rc<Timer>) {
        timer.insert_flags(Flags::LOWRES_DRIVER);
        TimerReset::register();

        crate::spawn(LowresTimerDriver {
            timer: timer.clone(),
            generation: timer.generation.get(),
            sleep: Delay::new(LOWRES_RESOLUTION),
        });
    }
}

impl Drop for LowresTimerDriver {
    fn drop(&mut self) {
        if self.timer.generation.get() == self.generation {
            self.timer.stop_lowres();
        }
    }
}

impl Future for LowresTimerDriver {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let timer = &this.timer;
        if timer.generation.get() != this.generation {
            return Poll::Ready(());
        }
        timer.lowres_driver.register(cx.waker());

        // the cache was populated, invalidate it after `LOWRES_RESOLUTION`
        let mut flags = timer.flags.get();
        if !flags.contains(Flags::LOWRES_TIMER) {
            flags.insert(Flags::LOWRES_TIMER);
            timer.flags.set(flags);
            this.sleep.reset(LOWRES_RESOLUTION);
        }

        if Pin::new(&mut this.sleep).poll(cx).is_ready() {
            timer.lowres_time.set(None);
            timer.lowres_stime.set(None);
            flags.remove(Flags::LOWRES_TIMER);
            timer.flags.set(flags);
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{Millis, interval, sleep};

    /// A handle dropped by a thread-local destructor after the wheel is
    /// destroyed must not panic.
    #[test]
    fn test_drop_handle_after_wheel_destroyed() {
        use std::cell::RefCell;

        thread_local! {
            static HOLDER: RefCell<Option<TimerHandle>> = const { RefCell::new(None) };
        }

        let res = std::thread::spawn(|| {
            // register the holder destructor first, the wheel is destroyed
            // before it, destructors run in reverse registration order
            HOLDER.with(|h| h.borrow_mut().take());
            ntex::rt::System::build()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async {
                    let hnd = TimerHandle::new(1000);
                    HOLDER.with(|h| *h.borrow_mut() = Some(hnd));
                });
        })
        .join();
        assert!(res.is_ok());
    }

    /// The drivers of a stopped system may never be dropped, the next system
    /// on the same thread starts its own drivers.
    #[test]
    fn test_timer_in_next_system() {
        let res = std::thread::spawn(|| {
            for _ in 0..3 {
                let start = Instant::now();
                ntex::rt::System::build()
                    .build(ntex::rt::DefaultRuntime)
                    .block_on(async {
                        sleep(Millis(10)).await;
                        let _ = now();
                        // pending timer of the stopped system
                        let _hnd = sleep(Millis(10_000));
                        crate::spawn(async { sleep(Millis(10_000)).await });
                    });
                assert!(start.elapsed() < Duration::from_secs(5));
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(res.join().is_ok()));
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true));
    }

    /// A stale driver does not stop the drivers of a newer generation.
    #[test]
    fn test_stale_driver_drop() {
        let timer = Rc::new(Timer::new());
        timer.insert_flags(Flags::RUNNING | Flags::DRIVER_STARTED | Flags::LOWRES_DRIVER);
        timer.reset();
        assert_eq!(timer.flags.get(), Flags::empty());

        timer.insert_flags(Flags::RUNNING | Flags::DRIVER_STARTED | Flags::LOWRES_DRIVER);
        drop(TimerDriver {
            timer: timer.clone(),
            generation: 0,
            sleep: Delay::new(Duration::ZERO),
            armed: None,
        });
        drop(LowresTimerDriver {
            timer: timer.clone(),
            generation: 0,
            sleep: Delay::new(Duration::ZERO),
        });
        assert_eq!(
            timer.flags.get(),
            Flags::RUNNING | Flags::DRIVER_STARTED | Flags::LOWRES_DRIVER
        );
    }

    fn entry() -> TimerEntry {
        TimerEntry {
            bucket: None,
            bucket_entry: 0,
            task: LocalWaker::new(),
        }
    }

    /// Bucket expiry is never earlier than the requested delay, and at most
    /// one level granularity later.
    #[test]
    fn test_bucket_expiry_bounds() {
        let timer = Timer::new();
        for elapsed in [0, 1, 7, 8, 63, 64, 511, 12_345, 1 << 30] {
            timer.elapsed.set(elapsed);
            let mut delta = 0;
            while delta < WHEEL_TIMEOUT_CUTOFF {
                let (idx, expiry) = timer.calc_wheel_index(elapsed + delta, delta);
                let lvl = (idx / LVL_SIZE as usize) as u64;
                assert!(expiry > elapsed + delta, "{elapsed} {delta} {expiry}");
                assert!(
                    expiry <= elapsed + delta + lvl_gran(lvl),
                    "{elapsed} {delta} {expiry}"
                );
                assert_eq!(expiry % lvl_gran(lvl), 0);
                assert_eq!(
                    ((expiry >> lvl_shift(lvl)) & LVL_MASK) as usize,
                    idx % LVL_SIZE as usize
                );
                delta = delta * 2 + 1;
            }

            // clamped to the capacity of the wheel
            let (_, expiry) =
                timer.calc_wheel_index(elapsed + u64::from(u32::MAX), u64::from(u32::MAX));
            assert!(expiry <= elapsed + WHEEL_TIMEOUT_CUTOFF);
        }
    }

    /// The next pending bucket is the earliest occupied one, and executing it
    /// wakes only its timers.
    #[test]
    fn test_next_pending_bucket() {
        let timer = Timer::new();
        timer.elapsed.set(1000);

        timer.with_wheel(|w| {
            assert_eq!(timer.next_pending_bucket(w), None);

            let mut expected = Vec::new();
            for delta in [5000, 30, 700, 100_000] {
                let (idx, expiry) = timer.calc_wheel_index(1000 + delta, delta);
                let no = w.timers.insert(entry());
                w.link(no, idx);
                expected.push((expiry, no));
            }
            expected.sort_unstable();

            for (expiry, no) in expected {
                assert_eq!(timer.next_pending_bucket(w), Some(expiry));
                timer.elapsed.set(expiry);
                w.execute_expired_timers(expiry);
                assert!(w.timers[no].bucket.is_none());
            }
            assert_eq!(timer.next_pending_bucket(w), None);
            assert_eq!(w.occupied, [0; LVL_DEPTH as usize]);
        });
    }

    #[test]
    fn test_unlink_relink() {
        let timer = Timer::new();
        timer.with_wheel(|w| {
            let a = w.timers.insert(entry());
            let b = w.timers.insert(entry());
            w.link(a, 3);
            w.link(b, 3);
            assert_eq!(w.occupied[0], 1 << 3);

            assert!(w.unlink(a));
            assert!(!w.unlink(a));
            assert_eq!(w.occupied[0], 1 << 3);

            w.relink(b, LVL_SIZE as usize + 5);
            assert_eq!(w.occupied[0], 0);
            assert_eq!(w.occupied[1], 1 << 5);
            assert_eq!(w.timers[b].bucket, Some(LVL_SIZE as u16 + 5));

            assert!(w.unlink(b));
            assert_eq!(w.occupied, [0; LVL_DEPTH as usize]);
        });
    }

    /// `reset(0)` elapses the timer and wakes the task waiting for it.
    #[ntex::test]
    async fn test_reset_zero_wakes_task() {
        let hnd = Rc::new(TimerHandle::new(10_000));
        let hnd2 = hnd.clone();
        crate::spawn(async move { hnd2.reset(0) });

        // the sleep wakes the task if `reset(0)` does not
        let start = Instant::now();
        crate::future::select(
            std::future::poll_fn(|cx| hnd.poll_elapsed(cx)),
            sleep(Millis(500)),
        )
        .await;
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(100), "elapsed: {elapsed:?}");
    }

    /// Dropping one driver does not reset the state of the other one.
    #[test]
    fn test_driver_drop_keeps_other_driver() {
        let timer = Rc::new(Timer::new());
        let no = timer.with_wheel(|w| {
            let no = w.timers.insert(entry());
            w.link(no, 3);
            no
        });
        timer.next_expiry.set(3);
        timer.lowres_time.set(Some(Instant::now()));
        timer.insert_flags(
            Flags::RUNNING | Flags::DRIVER_STARTED | Flags::LOWRES_DRIVER | Flags::LOWRES_TIMER,
        );

        drop(LowresTimerDriver {
            timer: timer.clone(),
            generation: 0,
            sleep: Delay::new(Duration::ZERO),
        });
        assert_eq!(timer.flags.get(), Flags::DRIVER_STARTED);
        assert!(timer.lowres_time.get().is_none());
        // timers are still pending
        assert_eq!(timer.next_expiry.get(), 3);
        timer.with_wheel(|w| assert_eq!(w.timers[no].bucket, Some(3)));

        timer.insert_flags(Flags::RUNNING | Flags::LOWRES_DRIVER);
        drop(TimerDriver {
            timer: timer.clone(),
            generation: 0,
            sleep: Delay::new(Duration::ZERO),
            armed: None,
        });
        assert_eq!(timer.flags.get(), Flags::LOWRES_DRIVER);
        assert_eq!(timer.next_expiry.get(), u64::MAX);
        timer.with_wheel(|w| {
            assert!(w.timers[no].bucket.is_none());
            assert_eq!(w.occupied, [0; LVL_DEPTH as usize]);
        });
    }

    /// A short timer is measured from the current time, not from the cached
    /// time that went stale while the thread was blocked.
    #[ntex::test]
    async fn test_short_timer_after_blocking() {
        let _hnd = sleep(Millis(10_000));
        let _ = now();

        // the lowres driver cannot run, the cached time goes stale
        std::thread::sleep(Duration::from_millis(100));

        let start = Instant::now();
        sleep(Millis(50)).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(50), "elapsed: {elapsed:?}");
    }

    /// A late wakeup must not delay the timers that expire later.
    #[ntex::test]
    async fn test_late_wakeup_does_not_drift() {
        let start = Instant::now();
        let fut1 = sleep(Millis(50));
        let fut2 = sleep(Millis(600));

        // block the thread, the driver wakes up ~450ms late for `fut1`.
        // `fut2` expires at ~620ms, with drift it would expire ~550ms after
        // the late wakeup, i.e. after ~1050ms. The bound leaves room for
        // scheduling latency on loaded machines.
        std::thread::sleep(Duration::from_millis(500));
        fut1.await;
        fut2.await;

        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(600) && elapsed < Duration::from_millis(900),
            "elapsed: {elapsed:?}"
        );
    }

    #[ntex::test]
    #[allow(unused_variables, clippy::used_underscore_binding)]
    async fn test_timer() {
        crate::spawn(async {
            let s = interval(Millis(25));
            loop {
                s.tick().await;
            }
        });
        let time = Instant::now();
        let fut1 = sleep(Millis(1000));
        let fut2 = sleep(Millis(200));

        fut2.await;
        #[cfg(not(target_os = "macos"))]
        {
            let _elapsed = time.elapsed();
            assert!(
                _elapsed > Duration::from_millis(200) && _elapsed < Duration::from_millis(300),
                "elapsed: {_elapsed:?}"
            );
        }

        fut1.await;

        #[cfg(not(target_os = "macos"))]
        {
            let _elapsed = time.elapsed();
            assert!(
                _elapsed > Duration::from_secs(1) && _elapsed < Duration::from_millis(1200), // osx
                "elapsed: {_elapsed:?}",
            );
        }

        let time = Instant::now();
        sleep(Millis(25)).await;
        #[cfg(not(target_os = "macos"))]
        {
            let _elapsed = time.elapsed();
            assert!(
                _elapsed > Duration::from_millis(20) && _elapsed < Duration::from_millis(50),
                "elapsed: {_elapsed:?}",
            );
        }
    }
}
