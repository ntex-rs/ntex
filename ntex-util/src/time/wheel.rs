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
//!   Wakers are invoked in place while the wheel is held; timer operations
//!   are not reentrant. Callback timers are queued separately and invoked
//!   after releasing the wheel.
//! * [`LowresTimerDriver`] invalidates the cached [`system_time()`] value
//!   every 150 milliseconds.
//!
//! Dropping the timer driver, i.e. when the runtime stops, stops the wheel and
//! marks all timers as elapsed. Dropping the lowres driver invalidates the
//! cached system time.
//!
//! A runtime does not always drop its pending tasks when it stops, the timer
//! is also reset once the arbiter storage of the stopped system is cleared,
//! so that the next runtime on the thread starts its own drivers. Drivers of
//! an older generation exit without touching the state.
//!
//! Registrations retain owned [`ntex_rt::LocalWaker`] handles, specializing
//! native owner-thread wakes without allocating wrappers or growing entries.
//! Standard wakers are still accepted at polling boundaries.
use std::num::NonZeroUsize;
use std::time::{Duration, Instant, SystemTime};
use std::{cell::Cell, cmp, future::Future, mem, pin::Pin, rc::Rc, task, task::Poll};

use futures_timer::Delay;
use slab::Slab;

use super::callbacks::Callbacks;
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

/// Returns the start of the current runtime turn, see [`ntex_rt::now()`].
///
/// Tasks polled in the same turn observe the same instant on neon and compio,
/// on tokio this is [`Instant::now()`].
#[inline]
pub fn now() -> Instant {
    ntex_rt::now()
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
///
/// Timer operations are not reentrant. Registered wakers must not synchronously
/// access the timer wheel when cloned, woken or dropped.
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
                let entry = &mut w.timers[self.0.get()];
                if entry.bucket.is_none() {
                    Poll::Ready(())
                } else {
                    if entry.task.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                        entry.task = Some(ntex_rt::LocalWaker::from_waker(cx.waker()));
                    }
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
        /// A timer was scheduled, `system_time()` caches the time.
        const RUNNING        = 0b0010_0000;
    }
}

thread_local! {
    pub(super) static TIMER: Rc<Timer> = Rc::new(Timer::new());
}

/// Per-thread timer state, shared by the timer handles and the driver tasks.
pub(super) struct Timer {
    /// Wheel clock in units, the expiry that was processed last.
    elapsed: Cell<u64>,
    /// Instant that corresponds to `elapsed`, the scheduled time of the last
    /// processed expiry. Set lazily when the wheel is idle.
    elapsed_time: Cell<Option<Instant>>,
    /// Expiry of the earliest occupied bucket, `u64::MAX` if the wheel is empty.
    next_expiry: Cell<u64>,
    flags: Cell<Flags>,
    /// Incremented on reset, drivers of an older generation are stale.
    pub(super) generation: Cell<u64>,
    driver: LocalWaker,
    lowres_stime: Cell<Option<SystemTime>>,
    lowres_driver: LocalWaker,
    /// Taken out of the cell for the duration of an operation.
    wheel: Cell<Option<Box<Wheel>>>,
}

/// Timer storage.
pub(super) struct Wheel {
    /// Timer entries indexed by handle, slot 0 is reserved so handles are
    /// non-zero.
    pub(super) timers: Slab<TimerEntry>,
    /// Handles of the timers stored in each bucket, `lvl * LVL_SIZE + offset`.
    buckets: Box<[Slab<usize>]>,
    /// Occupied buckets of each level, bit `n` is set if bucket `n` has timers.
    occupied: [u64; LVL_DEPTH as usize],
    /// Allocated only for callbacks, keeping ordinary entries waker-sized.
    pub(super) callbacks: Option<Box<Callbacks>>,
}

struct WheelGuard<'a> {
    slot: &'a Cell<Option<Box<Wheel>>>,
    wheel: Option<Box<Wheel>>,
}

impl Drop for WheelGuard<'_> {
    fn drop(&mut self) {
        self.slot.set(self.wheel.take());
    }
}

#[derive(Debug)]
pub(super) struct TimerEntry {
    /// Bucket index, `None` once the timer has elapsed.
    pub(super) bucket: Option<u16>,
    /// Key of the entry in the bucket.
    pub(super) bucket_entry: usize,
    pub(super) callback: bool,
    pub(super) task: Option<ntex_rt::LocalWaker>,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(mem::size_of::<TimerEntry>() == 32);

struct ExpiredTimers<'a> {
    timers: &'a mut Slab<TimerEntry>,
    entries: slab::Drain<'a, usize>,
}

impl Drop for ExpiredTimers<'_> {
    fn drop(&mut self) {
        // A panicking wake must not leave entries pointing into a drained bucket.
        for no in self.entries.by_ref() {
            self.timers[no].bucket = None;
        }
    }
}

impl Timer {
    fn new() -> Self {
        let mut timers = Slab::default();
        timers.insert(TimerEntry {
            bucket: None,
            bucket_entry: 0,
            callback: false,
            task: None,
        });

        Timer {
            elapsed: Cell::new(0),
            elapsed_time: Cell::new(None),
            next_expiry: Cell::new(u64::MAX),
            flags: Cell::new(Flags::empty()),
            generation: Cell::new(0),
            driver: LocalWaker::new(),
            lowres_stime: Cell::new(None),
            lowres_driver: LocalWaker::new(),
            wheel: Cell::new(Some(Box::new(Wheel {
                timers,
                buckets: (0..WHEEL_SIZE).map(|_| Slab::new()).collect(),
                occupied: [0; LVL_DEPTH as usize],
                callbacks: None,
            }))),
        }
    }

    pub(super) fn with_wheel<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Wheel) -> R,
    {
        let mut guard = WheelGuard {
            slot: &self.wheel,
            wheel: Some(
                self.wheel
                    .take()
                    .expect("timer wheel operations are not reentrant"),
            ),
        };
        f(guard.wheel.as_deref_mut().unwrap())
    }

    fn insert_flags(&self, flags: Flags) -> Flags {
        let mut f = self.flags.get();
        f.insert(flags);
        self.flags.set(f);
        f
    }

    /// Returns the cached system time, the cache is populated only while the
    /// wheel is running.
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
                    callback: false,
                    task: None,
                })
            })
        } else {
            let (idx, expiry) = self.calc_bucket(millis);
            let no = self.with_wheel(|w| {
                let no = w.timers.insert(TimerEntry {
                    bucket: None,
                    bucket_entry: 0,
                    callback: false,
                    task: None,
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
            if w.unlink(hnd)
                && let Some(task) = w.timers[hnd].task.take()
            {
                task.wake();
            }
        });
    }

    /// Returns the bucket index and the bucket expiry for a delay starting now.
    pub(super) fn calc_bucket(self: &Rc<Self>, millis: u64) -> (usize, u64) {
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
    pub(super) fn update_next_expiry(self: &Rc<Self>, expiry: u64) {
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
        self.lowres_stime.set(None);
    }

    /// Stops the drivers of the current generation, called when the system
    /// that runs them has stopped.
    fn reset(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.stop_wheel();
        self.stop_lowres();
        self.driver.take_local();
        self.lowres_driver.take_local();
    }

    /// Marks all timers as elapsed and resets the wheel, called when the timer
    /// driver is dropped. Tasks are not woken, the runtime is stopping.
    fn stop_wheel(&self) {
        self.remove_flags(Flags::DRIVER_STARTED);

        self.with_wheel(|wheel| {
            let Wheel {
                timers,
                buckets,
                occupied,
                callbacks,
            } = wheel;
            for bucket in buckets.iter_mut() {
                for no in bucket.drain() {
                    timers[no].bucket = None;
                }
            }
            *occupied = [0; LVL_DEPTH as usize];
            if let Some(callbacks) = callbacks {
                callbacks.ready.clear();
                for ticket in callbacks.entries.values() {
                    if let Some(state) = ticket.state.upgrade() {
                        state.pending.set(false);
                    }
                }
            }

            self.next_expiry.set(u64::MAX);
            self.elapsed.set(0);
            self.elapsed_time.set(None);
        });
    }
}

impl Wheel {
    /// Level and occupied bit of a bucket.
    fn bucket_bit(idx: usize) -> (usize, u64) {
        (idx / LVL_SIZE as usize, 1 << (idx % LVL_SIZE as usize))
    }

    /// Stores the timer in bucket `idx`.
    pub(super) fn link(&mut self, hnd: usize, idx: usize) {
        let entry = &mut self.timers[hnd];
        entry.bucket = Some(idx as u16);
        entry.bucket_entry = self.buckets[idx].insert(hnd);

        let (lvl, bit) = Self::bucket_bit(idx);
        self.occupied[lvl] |= bit;
    }

    /// Removes the timer from its bucket and marks it as elapsed.
    ///
    /// Returns `false` if the timer has already elapsed.
    pub(super) fn unlink(&mut self, hnd: usize) -> bool {
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
    pub(super) fn relink(&mut self, hnd: usize, idx: usize) {
        if self.timers[hnd].bucket != Some(idx as u16) {
            self.unlink(hnd);
            self.link(hnd, idx);
        }
    }

    /// Marks timers elapsed, waking tasks in place and queueing only callbacks.
    fn execute_expired_timers(&mut self, mut clk: u64) {
        for lvl in 0..LVL_DEPTH {
            let idx = ((clk & LVL_MASK) + lvl * LVL_SIZE) as usize;
            let bucket = &mut self.buckets[idx];
            if !bucket.is_empty() {
                let (lvl, bit) = Self::bucket_bit(idx);
                self.occupied[lvl] &= !bit;
                let mut expired = ExpiredTimers {
                    timers: &mut self.timers,
                    entries: bucket.drain(),
                };
                for no in expired.entries.by_ref() {
                    let entry = &mut expired.timers[no];
                    entry.bucket = None;
                    if entry.callback {
                        let callbacks = self.callbacks.as_mut().unwrap();
                        callbacks.ready.push(callbacks.entries[&no].clone());
                    } else if let Some(task) = entry.task.take() {
                        task.wake();
                    }
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
        crate::spawn_detached(TimerDriver {
            timer: timer.clone(),
            generation: timer.generation.get(),
            sleep: Delay::new(deadline.saturating_duration_since(Instant::now())),
            armed: Some(deadline),
        });
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

        let mut now = Instant::now();

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

            let (next, callbacks) = timer.with_wheel(|w| {
                w.execute_expired_timers(expiry);
                let callbacks = w.callbacks.as_mut().and_then(|callbacks| {
                    if callbacks.ready.is_empty() {
                        None
                    } else {
                        Some(mem::take(&mut callbacks.ready))
                    }
                });
                (timer.next_pending_bucket(w), callbacks)
            });

            if let Some(next) = next {
                timer.next_expiry.set(next);
            } else {
                timer.next_expiry.set(u64::MAX);
                timer.elapsed_time.set(None);
            }

            if let Some(mut callbacks) = callbacks {
                for ticket in callbacks.drain(..) {
                    if timer.generation.get() != this.generation {
                        return Poll::Ready(());
                    }
                    if ticket.generation == this.generation
                        && let Some(state) = ticket.state.upgrade()
                        && state.arm.get() == ticket.arm
                        && state.pending.replace(false)
                    {
                        (state.callback)();
                    }
                }
                // A callback or its captures may have stopped this runtime.
                if timer.generation.get() != this.generation {
                    return Poll::Ready(());
                }
                timer.with_wheel(|wheel| wheel.callbacks.as_mut().unwrap().ready = callbacks);
                now = Instant::now();
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

        crate::spawn_detached(LowresTimerDriver {
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
            timer.lowres_stime.set(None);
            flags.remove(Flags::LOWRES_TIMER);
            timer.flags.set(flags);
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, RawWaker, RawWakerVTable, Wake, Waker};

    use super::*;
    use crate::future::OnDropFn;
    use crate::time::{CallbackTimer, Millis, interval, sleep};

    #[derive(Default)]
    struct InPlaceWaker {
        wakes: AtomicUsize,
        delay: Duration,
        panic_on_wake: bool,
    }

    impl Wake for InPlaceWaker {
        fn wake(self: Arc<Self>) {
            TIMER.with(|timer| {
                let wheel = timer.wheel.take();
                let held = wheel.is_none();
                timer.wheel.set(wheel);
                assert!(held, "waker must run in place while the wheel is held");
            });
            let previous = self.wakes.fetch_add(1, Ordering::Relaxed);
            assert!(!self.panic_on_wake, "notification");
            if previous == 0 && !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
        }
    }

    fn timer_state() -> Rc<Timer> {
        TIMER.with(|timer| {
            timer.reset();
            // These tests drive expiry manually, without an executor.
            timer.insert_flags(Flags::DRIVER_STARTED);
            timer.clone()
        })
    }

    fn timer_driver(timer: &Rc<Timer>) -> TimerDriver {
        TimerDriver {
            timer: timer.clone(),
            generation: timer.generation.get(),
            sleep: Delay::new(Duration::ZERO),
            armed: None,
        }
    }

    fn make_due(timer: &Timer) {
        timer.elapsed_time.set(Some(
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        ));
    }

    #[derive(Debug)]
    struct NoopNotify;

    impl ntex_rt::Notify for NoopNotify {
        fn notify(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_local_waker_registration() {
        struct Registration {
            inner: Waker,
            clones: AtomicUsize,
            panic_on_clone: AtomicBool,
        }

        // Keep clone identity stable without relying on generic vtable deduplication.
        static VTABLE: RawWakerVTable = RawWakerVTable::new(
            |data| unsafe {
                let state = &*data.cast::<Registration>();
                assert!(!state.panic_on_clone.load(Ordering::Relaxed), "clone");
                state.clones.fetch_add(1, Ordering::Relaxed);
                Arc::increment_strong_count(data.cast::<Registration>());
                RawWaker::new(data, &VTABLE)
            },
            |data| unsafe {
                Arc::from_raw(data.cast::<Registration>())
                    .inner
                    .wake_by_ref();
            },
            |data| unsafe {
                (*data.cast::<Registration>()).inner.wake_by_ref();
            },
            |data| unsafe {
                drop(Arc::from_raw(data.cast::<Registration>()));
            },
        );

        let wrap = |task: &Arc<InPlaceWaker>| {
            let state = Arc::new(Registration {
                inner: Waker::from(task.clone()),
                clones: AtomicUsize::new(0),
                panic_on_clone: AtomicBool::new(false),
            });
            let waker = unsafe {
                Waker::from_raw(RawWaker::new(Arc::into_raw(state.clone()).cast(), &VTABLE))
            };
            (state, waker)
        };

        let timer = timer_state();
        let handle = TimerHandle::new(10_000);
        let old_task = Arc::new(InPlaceWaker::default());
        let new_task = Arc::new(InPlaceWaker::default());
        let (old, old_waker) = wrap(&old_task);
        let (new, new_waker) = wrap(&new_task);
        let mut old_cx = Context::from_waker(&old_waker);
        let mut new_cx = Context::from_waker(&new_waker);

        assert!(handle.poll_elapsed(&mut old_cx).is_pending());
        handle.reset(20_000);
        assert!(handle.poll_elapsed(&mut old_cx).is_pending());
        assert_eq!(old.clones.load(Ordering::Relaxed), 1);
        assert_eq!(Arc::strong_count(&old), 3);

        new.panic_on_clone.store(true, Ordering::Relaxed);
        assert!(catch_unwind(AssertUnwindSafe(|| handle.poll_elapsed(&mut new_cx))).is_err());
        timer.with_wheel(|wheel| {
            assert!(
                wheel.timers[handle.0.get()]
                    .task
                    .as_ref()
                    .unwrap()
                    .will_wake(&old_waker)
            );
        });
        assert_eq!(Arc::strong_count(&old), 3);
        assert_eq!(Arc::strong_count(&new), 2);

        new.panic_on_clone.store(false, Ordering::Relaxed);
        assert!(handle.poll_elapsed(&mut new_cx).is_pending());
        assert!(handle.poll_elapsed(&mut new_cx).is_pending());
        assert_eq!(new.clones.load(Ordering::Relaxed), 1);
        assert_eq!(Arc::strong_count(&old), 2);
        assert_eq!(Arc::strong_count(&new), 3);

        timer.with_wheel(|wheel| {
            wheel.relink(handle.0.get(), 3);
            wheel.execute_expired_timers(3);
        });
        assert!(handle.poll_elapsed(&mut new_cx).is_ready());
        assert_eq!(new.clones.load(Ordering::Relaxed), 1);
        assert_eq!(new_task.wakes.load(Ordering::Relaxed), 1);
        assert_eq!(old_task.wakes.load(Ordering::Relaxed), 0);
        assert_eq!(Arc::strong_count(&new), 2);

        handle.reset(10_000);
        assert!(handle.poll_elapsed(&mut old_cx).is_pending());
        handle.reset(0);
        assert!(handle.is_elapsed());
        assert_eq!(old_task.wakes.load(Ordering::Relaxed), 1);
        assert_eq!(Arc::strong_count(&old), 2);

        handle.reset(10_000);
        assert!(handle.poll_elapsed(&mut old_cx).is_pending());
        drop(handle);
        assert_eq!(old_task.wakes.load(Ordering::Relaxed), 1);
        assert_eq!(old.clones.load(Ordering::Relaxed), 3);
        assert_eq!(Arc::strong_count(&old), 2);
    }

    #[test]
    fn test_local_waker_native_replacement() {
        let timer = timer_state();
        let rt = ntex_rt::Runtime::new(Box::new(NoopNotify));
        let handle = Rc::new(TimerHandle::new(10_000));
        let first_waker = Rc::new(Cell::new(None));
        let (first_handle, saved) = (handle.clone(), first_waker.clone());
        let first = rt.spawn(std::future::poll_fn(move |cx| {
            saved.set(Some(cx.waker().clone()));
            first_handle.poll_elapsed(cx)
        }));
        rt.poll();

        let second_handle = handle.clone();
        let second = rt.spawn(std::future::poll_fn(move |cx| {
            second_handle.poll_elapsed(cx)
        }));
        rt.poll();
        timer.with_wheel(|wheel| {
            wheel.relink(handle.0.get(), 3);
            wheel.execute_expired_timers(3);
        });
        rt.poll();
        assert!(!first.is_finished());
        assert!(second.is_finished());

        handle.reset(10_000);
        first_waker.take().unwrap().wake();
        rt.poll();
        assert!(!first.is_finished());
        handle.reset(0);
        rt.poll();
        assert!(first.is_finished());
        assert_eq!(Rc::strong_count(&handle), 1);
        drop(rt);
        first_waker.take().unwrap().wake();
    }

    #[test]
    fn test_local_waker_runtime_shutdown() {
        for reuse in [false, true] {
            let timer = timer_state();
            let rt = ntex_rt::Runtime::new(Box::new(NoopNotify));
            let handle = Rc::new(TimerHandle::new(10_000));
            let (task_timer, task_handle) = (timer.clone(), handle.clone());
            let task = rt.spawn(std::future::poll_fn(move |cx| {
                task_timer.driver.register(cx.waker());
                task_timer.lowres_driver.register(cx.waker());
                task_handle.poll_elapsed(cx)
            }));
            rt.poll();
            drop(rt);
            assert!(task.is_finished());
            assert_eq!(Rc::strong_count(&handle), 1);
            drop(task);

            timer.reset();
            assert!(handle.is_elapsed());
            assert!(!timer.driver.is_set());
            assert!(!timer.lowres_driver.is_set());
            timer.with_wheel(|wheel| {
                assert!(wheel.timers[handle.0.get()].task.is_some());
            });

            if reuse {
                timer.insert_flags(Flags::DRIVER_STARTED);
                handle.reset(10_000);
                let rt = ntex_rt::Runtime::new(Box::new(NoopNotify));
                let task_handle = handle.clone();
                let task = rt.spawn(std::future::poll_fn(move |cx| task_handle.poll_elapsed(cx)));
                rt.poll();
                assert!(!task.is_finished());
                handle.elapse();
                rt.poll();
                assert!(task.is_finished());
                assert_eq!(Rc::strong_count(&handle), 1);
            }
            drop(handle);
            timer.with_wheel(|wheel| assert_eq!(wheel.timers.len(), 1));
        }
    }

    #[test]
    fn test_local_waker_in_next_system() {
        struct Wrapped {
            inner: Waker,
            wakes: Arc<AtomicUsize>,
        }

        impl Wake for Wrapped {
            fn wake(self: Arc<Self>) {
                self.wakes.fetch_add(1, Ordering::Relaxed);
                self.inner.wake_by_ref();
            }
        }

        std::thread::spawn(|| {
            let mut handle = None::<TimerHandle>;
            for wrapped in [false, true, false] {
                let notifications = Arc::new(AtomicUsize::new(0));
                let count = notifications.clone();
                handle = Some(
                    ntex::rt::System::build()
                        .build(ntex::rt::DefaultRuntime)
                        .block_on(async move {
                            let handle = if let Some(handle) = handle {
                                assert!(handle.is_elapsed());
                                handle.reset(10_000);
                                handle
                            } else {
                                TimerHandle::new(10_000)
                            };
                            std::future::poll_fn(|cx| {
                                let waker = if wrapped {
                                    Waker::from(Arc::new(Wrapped {
                                        inner: cx.waker().clone(),
                                        wakes: count.clone(),
                                    }))
                                } else {
                                    cx.waker().clone()
                                };
                                let cx = &mut Context::from_waker(&waker);
                                assert!(handle.poll_elapsed(cx).is_pending());
                                handle.reset(0);
                                assert!(handle.poll_elapsed(cx).is_ready());
                                handle.reset(10_000);
                                assert!(handle.poll_elapsed(cx).is_pending());
                                Poll::Ready(())
                            })
                            .await;
                            handle
                        }),
                );
                assert!(handle.as_ref().unwrap().is_elapsed());
                assert_eq!(notifications.load(Ordering::Relaxed), usize::from(wrapped));
            }
        })
        .join()
        .unwrap();
    }

    #[ntex::test]
    async fn test_callback_timer() {
        let calls = Rc::new(Cell::new(0));
        let count = calls.clone();
        let callback = crate::time::CallbackTimer::new(Millis::ZERO, move || {
            count.set(count.get() + 1);
        });
        assert!(!callback.is_elapsed());
        assert_eq!(calls.get(), 0);
        callback.reset(Millis(10_000));
        callback.cancel();
        assert!(callback.is_elapsed());
        sleep(Millis(50)).await;
        assert_eq!(calls.get(), 0);

        for expected in 1..=2 {
            callback.reset(Millis::ZERO);
            assert!(!callback.is_elapsed());
            sleep(Millis(50)).await;
            assert!(callback.is_elapsed());
            assert_eq!(calls.get(), expected);
        }
    }

    #[test]
    fn test_callback_stale_tickets() {
        let timer = timer_state();
        let calls = Rc::new(Cell::new(0));
        let count = calls.clone();
        let cancelled = CallbackTimer::new(Millis(1), move || count.set(count.get() + 1));
        let count = calls.clone();
        let reset = CallbackTimer::new(Millis(1), move || count.set(count.get() + 1));
        let capture = Rc::new(Cell::new(0));
        let weak = Rc::downgrade(&capture);
        let dropped = CallbackTimer::new(Millis(1), move || capture.set(1));
        let old_slot = dropped.hnd;
        timer.with_wheel(|wheel| {
            for handle in [&cancelled, &reset, &dropped] {
                wheel.relink(handle.hnd.get(), 3);
            }
            wheel.execute_expired_timers(3);
        });
        assert!(cancelled.is_elapsed());
        assert!(reset.is_elapsed());
        assert!(dropped.is_elapsed());
        assert_eq!(calls.get(), 0);
        cancelled.cancel();
        reset.reset(Millis(10_000));
        drop(dropped);
        assert!(
            weak.upgrade().is_none(),
            "queued tickets must not retain captures"
        );
        let count = calls.clone();
        let replacement = CallbackTimer::new(Millis(10_000), move || count.set(count.get() + 100));
        assert_eq!(replacement.hnd, old_slot);

        make_due(&timer);
        let mut driver = timer_driver(&timer);
        assert!(
            Pin::new(&mut driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(calls.get(), 0);
        assert!(!reset.is_elapsed());
        assert!(!replacement.is_elapsed());
        timer.elapsed_time.set(Some(
            Instant::now().checked_sub(Duration::from_secs(20)).unwrap(),
        ));
        for _ in 0..2 {
            assert!(
                Pin::new(&mut driver)
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(calls.get(), 101);
        }
    }

    #[test]
    fn test_callback_self_rearm_and_cross_cancel() {
        let timer = timer_state();
        let calls = Rc::new(Cell::new(0));
        let handles = Rc::new(RefCell::new(Vec::<CallbackTimer>::new()));
        for index in 0..2 {
            let count = calls.clone();
            let weak = Rc::downgrade(&handles);
            let callback = CallbackTimer::new(Millis(1), move || {
                count.set(count.get() + 1);
                let handles = weak.upgrade().unwrap();
                let handles = handles.borrow();
                assert!(handles[index].is_elapsed());
                handles[1 - index].cancel();
                handles[index].reset(Millis(10_000));
            });
            handles.borrow_mut().push(callback);
        }
        timer.with_wheel(|wheel| {
            for handle in handles.borrow().iter() {
                wheel.relink(handle.hnd.get(), 1);
            }
        });
        make_due(&timer);
        let mut driver = timer_driver(&timer);
        assert!(
            Pin::new(&mut driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            handles.borrow().iter().filter(|h| !h.is_elapsed()).count(),
            1
        );
        timer.elapsed_time.set(Some(
            Instant::now().checked_sub(Duration::from_secs(20)).unwrap(),
        ));
        for _ in 0..2 {
            assert!(
                Pin::new(&mut driver)
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(calls.get(), 2);
        }
    }

    #[test]
    fn test_callback_capture_drop_schedules_timers() {
        for active in [false, true] {
            let timer = timer_state();
            let released = Rc::new(Cell::new(false));
            let finished = Rc::new(Cell::new(false));
            let reset_calls = Rc::new(Cell::new(0));
            let new_calls = Rc::new(Cell::new(0));
            let count = reset_calls.clone();
            let reset = Rc::new(CallbackTimer::new(Millis(20_000), move || {
                count.set(count.get() + 1);
            }));
            let cancelled = Rc::new(CallbackTimer::new(Millis(20_000), || {
                panic!("cancelled callback");
            }));
            let replacement = Rc::new(RefCell::new(None::<CallbackTimer>));
            let capture = {
                let reset = reset.clone();
                let cancelled = cancelled.clone();
                let replacement = replacement.clone();
                let new_calls = new_calls.clone();
                let released = released.clone();
                let finished = finished.clone();
                OnDropFn::new(move || {
                    assert!(!active || finished.get());
                    let ordinary = TimerHandle::new(10_000);
                    ordinary.reset(0);
                    drop(ordinary);
                    cancelled.cancel();
                    reset.reset(Millis(10_000));
                    *replacement.borrow_mut() =
                        Some(CallbackTimer::new(Millis(10_000), move || {
                            new_calls.set(new_calls.get() + 1);
                        }));
                    released.set(true);
                })
            };
            let handle = Rc::new(RefCell::new(None::<CallbackTimer>));
            let weak = Rc::downgrade(&handle);
            let was_released = released.clone();
            *handle.borrow_mut() = Some(CallbackTimer::new(Millis(1), move || {
                let _capture = &capture;
                assert!(active, "dropped queued callback must not run");
                drop(weak.upgrade().unwrap().borrow_mut().take());
                assert!(!was_released.get());
                finished.set(true);
            }));
            let old_slot = handle.borrow().as_ref().unwrap().hnd;
            if !active {
                timer.with_wheel(|wheel| {
                    wheel.relink(old_slot.get(), 1);
                    wheel.execute_expired_timers(1);
                });
                drop(handle.borrow_mut().take());
                assert!(released.get());
            }
            make_due(&timer);
            let mut driver = timer_driver(&timer);
            assert!(
                Pin::new(&mut driver)
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert!(released.get());
            assert!(handle.borrow().is_none());
            assert!(cancelled.is_elapsed());
            assert!(!reset.is_elapsed());
            assert_eq!(replacement.borrow().as_ref().unwrap().hnd, old_slot);
            assert!(!replacement.borrow().as_ref().unwrap().is_elapsed());
            assert_eq!((reset_calls.get(), new_calls.get()), (0, 0));

            timer.elapsed_time.set(Some(
                Instant::now().checked_sub(Duration::from_secs(20)).unwrap(),
            ));
            for _ in 0..2 {
                assert!(
                    Pin::new(&mut driver)
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                assert_eq!((reset_calls.get(), new_calls.get()), (1, 1));
            }
        }
    }

    #[test]
    fn test_callback_capture_drop_resets_runtime() {
        let timer = timer_state();
        let new_calls = Rc::new(Cell::new(0));
        let replacement = Rc::new(RefCell::new(None::<CallbackTimer>));
        let capture = {
            let timer = timer.clone();
            let new_calls = new_calls.clone();
            let replacement = replacement.clone();
            OnDropFn::new(move || {
                timer.reset();
                timer.insert_flags(Flags::DRIVER_STARTED);
                let callback = CallbackTimer::new(Millis(1), move || {
                    new_calls.set(new_calls.get() + 1);
                });
                timer.with_wheel(|wheel| {
                    wheel.relink(callback.hnd.get(), 1);
                    wheel.execute_expired_timers(1);
                });
                *replacement.borrow_mut() = Some(callback);
            })
        };
        let handle = Rc::new(RefCell::new(None::<CallbackTimer>));
        let weak = Rc::downgrade(&handle);
        *handle.borrow_mut() = Some(CallbackTimer::new(Millis(1), move || {
            let _capture = &capture;
            drop(weak.upgrade().unwrap().borrow_mut().take());
        }));
        make_due(&timer);
        let mut old_driver = timer_driver(&timer);
        assert!(
            Pin::new(&mut old_driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert!(handle.borrow().is_none());
        assert!(replacement.borrow().as_ref().unwrap().is_elapsed());
        assert_eq!(new_calls.get(), 0);
        drop(old_driver);

        make_due(&timer);
        let mut driver = timer_driver(&timer);
        for _ in 0..2 {
            assert!(
                Pin::new(&mut driver)
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(new_calls.get(), 1);
        }
    }

    #[test]
    fn test_callback_refreshes_time() {
        let timer = timer_state();
        let calls = Rc::new(Cell::new(0));
        let handles = [1, 32].map(|millis| {
            let calls = calls.clone();
            CallbackTimer::new(Millis(millis), move || {
                let previous = calls.replace(calls.get() + 1);
                if previous == 0 {
                    std::thread::sleep(Duration::from_millis(64));
                }
            })
        });
        timer.with_wheel(|wheel| {
            wheel.relink(handles[0].hnd.get(), 1);
            wheel.relink(handles[1].hnd.get(), 3);
        });
        timer.elapsed_time.set(Some(
            Instant::now()
                .checked_sub(Duration::from_millis(32))
                .unwrap(),
        ));
        let mut driver = timer_driver(&timer);
        assert!(
            Pin::new(&mut driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(calls.get(), 2);
        assert!(handles.iter().all(CallbackTimer::is_elapsed));
    }

    #[test]
    fn test_callback_panic_after_rearm_cleanup() {
        let timer = timer_state();
        let callback = Rc::new_cyclic(|weak: &std::rc::Weak<CallbackTimer>| {
            let weak = weak.clone();
            CallbackTimer::new(Millis(1), move || {
                let callback = weak.upgrade().unwrap();
                callback.reset(Millis(10_000));
                panic!("callback");
            })
        });
        let later = CallbackTimer::new(Millis(10_000), || panic!("not due"));
        let ordinary = TimerHandle::new(10_000);
        make_due(&timer);
        let mut driver = timer_driver(&timer);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = Pin::new(&mut driver).poll(&mut Context::from_waker(Waker::noop()));
            }))
            .is_err()
        );
        assert!(!callback.is_elapsed());
        drop(driver);
        assert!(callback.is_elapsed());
        assert!(!callback.state.pending.get());
        assert!(later.is_elapsed());
        assert!(ordinary.is_elapsed());
        assert!(!later.state.pending.get());
        timer.with_wheel(|wheel| {
            assert!(wheel.buckets.iter().all(Slab::is_empty));
            assert_eq!(wheel.occupied, [0; LVL_DEPTH as usize]);
            assert!(wheel.callbacks.as_ref().unwrap().ready.is_empty());
        });
    }

    #[test]
    fn test_callback_runtime_reset_during_delivery() {
        let timer = timer_state();
        let calls = Rc::new(Cell::new(0));
        let new_calls = Rc::new(Cell::new(0));
        let replacement = Rc::new(RefCell::new(None::<CallbackTimer>));
        let handles = [0, 1].map(|_| {
            let timer = timer.clone();
            let calls = calls.clone();
            let new_calls = new_calls.clone();
            let replacement = replacement.clone();
            CallbackTimer::new(Millis(1), move || {
                calls.set(calls.get() + 1);
                timer.reset();
                timer.insert_flags(Flags::DRIVER_STARTED);
                let new_calls = new_calls.clone();
                *replacement.borrow_mut() = Some(CallbackTimer::new(Millis(1), move || {
                    new_calls.set(new_calls.get() + 1);
                }));
            })
        });
        timer.with_wheel(|wheel| {
            for handle in &handles {
                wheel.relink(handle.hnd.get(), 1);
            }
        });
        make_due(&timer);
        let mut old_driver = timer_driver(&timer);
        assert!(
            Pin::new(&mut old_driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert_eq!(calls.get(), 1);
        assert!(handles.iter().all(CallbackTimer::is_elapsed));
        drop(old_driver);
        assert!(!replacement.borrow().as_ref().unwrap().is_elapsed());
        make_due(&timer);
        let mut driver = timer_driver(&timer);
        assert!(
            Pin::new(&mut driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(new_calls.get(), 1);
    }

    #[test]
    fn test_callback_mixed_bucket_waker_panic() {
        for panic_position in 0..3 {
            let timer = timer_state();
            let calls = Rc::new(Cell::new(0));
            let callbacks = [0, 1].map(|_| {
                let calls = calls.clone();
                CallbackTimer::new(Millis(10_000), move || calls.set(calls.get() + 1))
            });
            let ordinary = TimerHandle::new(10_000);
            let task = Arc::new(InPlaceWaker {
                panic_on_wake: true,
                ..InPlaceWaker::default()
            });
            let waker = Waker::from(task.clone());
            assert!(
                ordinary
                    .poll_elapsed(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            timer.with_wheel(|wheel| {
                // Control slab drain positions, not public callback delivery order.
                let mut entries = [
                    callbacks[0].hnd.get(),
                    ordinary.0.get(),
                    callbacks[1].hnd.get(),
                ];
                entries.swap(1, panic_position);
                for no in entries {
                    wheel.unlink(no);
                }
                wheel.buckets[3] = Slab::new();
                for no in entries {
                    wheel.link(no, 3);
                }
            });
            timer.next_expiry.set(3);
            make_due(&timer);
            let mut driver = timer_driver(&timer);
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    let _ = Pin::new(&mut driver).poll(&mut Context::from_waker(Waker::noop()));
                }))
                .is_err()
            );
            assert_eq!(task.wakes.load(Ordering::Relaxed), 1);
            assert_eq!(calls.get(), 0);
            assert!(ordinary.is_elapsed());
            assert!(callbacks.iter().all(CallbackTimer::is_elapsed));
            timer.with_wheel(|wheel| {
                assert!(wheel.buckets.iter().all(Slab::is_empty));
                assert_eq!(wheel.occupied, [0; LVL_DEPTH as usize]);
                assert_eq!(
                    wheel.callbacks.as_ref().unwrap().ready.len(),
                    panic_position
                );
            });
            drop(driver);
            assert!(
                callbacks
                    .iter()
                    .all(|callback| !callback.state.pending.get())
            );
            timer.with_wheel(|wheel| {
                assert!(wheel.callbacks.as_ref().unwrap().ready.is_empty());
            });

            timer.insert_flags(Flags::DRIVER_STARTED);
            callbacks[0].reset(Millis::ZERO);
            make_due(&timer);
            let mut driver = timer_driver(&timer);
            for _ in 0..2 {
                assert!(
                    Pin::new(&mut driver)
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                assert_eq!(calls.get(), 1);
            }
        }
    }

    #[test]
    fn test_callback_reuse_in_next_system() {
        std::thread::spawn(|| {
            let calls = Rc::new(Cell::new(0));
            let mut callback = None::<CallbackTimer>;
            for expected in 1..=2 {
                let calls = calls.clone();
                callback = Some(
                    ntex::rt::System::build()
                        .build(ntex::rt::DefaultRuntime)
                        .block_on(async move {
                            let callback = if let Some(callback) = callback {
                                assert!(callback.is_elapsed());
                                sleep(Millis(50)).await;
                                assert_eq!(calls.get(), expected - 1);
                                callback.reset(Millis::ZERO);
                                callback
                            } else {
                                let calls = calls.clone();
                                CallbackTimer::new(Millis::ZERO, move || {
                                    calls.set(calls.get() + 1);
                                })
                            };
                            sleep(Millis(50)).await;
                            assert_eq!(calls.get(), expected);
                            callback.reset(Millis(10_000));
                            callback
                        }),
                );
                assert!(callback.as_ref().unwrap().is_elapsed());
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn test_wheel_access_restored_on_unwind() {
        let timer = Timer::new();
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                timer.with_wheel(|wheel| {
                    wheel.timers.insert(entry());
                    panic!("wheel operation");
                });
            }))
            .is_err()
        );
        timer.with_wheel(|wheel| assert_eq!(wheel.timers.len(), 2));
    }

    #[test]
    fn test_wake_in_place() {
        let timer = timer_state();
        let task = Arc::new(InPlaceWaker::default());
        let waker = Waker::from(task.clone());
        let mut cx = Context::from_waker(&waker);
        for reset in [false, true] {
            let handle = TimerHandle::new(10_000);
            assert!(handle.poll_elapsed(&mut cx).is_pending());
            if reset {
                handle.reset(0);
            } else {
                handle.elapse();
            }
            assert!(handle.is_elapsed());
        }
        let handles = [TimerHandle::new(1), TimerHandle::new(1)];
        for handle in &handles {
            assert!(handle.poll_elapsed(&mut cx).is_pending());
        }
        timer.elapsed_time.set(Some(
            Instant::now().checked_sub(Duration::from_secs(20)).unwrap(),
        ));
        let mut driver = timer_driver(&timer);
        assert!(
            Pin::new(&mut driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(handles.iter().all(TimerHandle::is_elapsed));
        assert_eq!(task.wakes.load(Ordering::Relaxed), 4);
        assert_eq!(Arc::strong_count(&task), 2);
    }

    #[test]
    fn test_dispatch_uses_one_time_sample_for_wakers() {
        let timer = timer_state();
        let handles = [TimerHandle::new(1), TimerHandle::new(32)];
        timer.with_wheel(|wheel| wheel.relink(handles[1].0.get(), 3));
        let task = Arc::new(InPlaceWaker {
            delay: Duration::from_millis(64),
            ..InPlaceWaker::default()
        });
        let waker = Waker::from(task.clone());
        for handle in &handles {
            assert!(
                handle
                    .poll_elapsed(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        }
        timer.elapsed_time.set(Some(
            Instant::now()
                .checked_sub(Duration::from_millis(32))
                .unwrap(),
        ));
        let mut driver = timer_driver(&timer);
        assert!(
            Pin::new(&mut driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(handles[0].is_elapsed());
        assert!(!handles[1].is_elapsed());
        assert_eq!(task.wakes.load(Ordering::Relaxed), 1);
        assert!(
            Pin::new(&mut driver)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(handles[1].is_elapsed());
        assert_eq!(task.wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_dispatch_panic_cleans_expired_bucket() {
        let timer = timer_state();
        let handles = [TimerHandle::new(1), TimerHandle::new(1)];
        timer.with_wheel(|wheel| {
            let bucket = wheel.timers[handles[0].0.get()].bucket.unwrap();
            wheel.relink(handles[1].0.get(), usize::from(bucket));
        });
        let task = Arc::new(InPlaceWaker {
            panic_on_wake: true,
            ..InPlaceWaker::default()
        });
        let waker = Waker::from(task.clone());
        for handle in &handles {
            assert!(
                handle
                    .poll_elapsed(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        }
        timer.elapsed_time.set(Some(
            Instant::now()
                .checked_sub(Duration::from_millis(64))
                .unwrap(),
        ));
        let mut driver = timer_driver(&timer);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = Pin::new(&mut driver).poll(&mut Context::from_waker(Waker::noop()));
            }))
            .is_err()
        );
        assert_eq!(task.wakes.load(Ordering::Relaxed), 1);
        assert!(handles.iter().all(TimerHandle::is_elapsed));
        timer.with_wheel(|wheel| {
            assert!(wheel.buckets.iter().all(Slab::is_empty));
            assert_eq!(wheel.occupied, [0; LVL_DEPTH as usize]);
        });
        for handle in &handles {
            handle.reset(10_000);
            assert!(!handle.is_elapsed());
        }
        drop(driver);
        assert!(handles.iter().all(TimerHandle::is_elapsed));
        assert!(!timer.flags.get().contains(Flags::DRIVER_STARTED));
    }

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
                    std::future::poll_fn(|cx| {
                        assert!(hnd.poll_elapsed(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                    HOLDER.with(|h| *h.borrow_mut() = Some(hnd));
                });
        })
        .join();
        assert!(res.is_ok());
    }

    #[test]
    fn test_drop_callback_after_wheel_destroyed() {
        thread_local! {
            static HOLDER: RefCell<Option<CallbackTimer>> = const { RefCell::new(None) };
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let destroyed = Arc::new(AtomicBool::new(false));
        let count = drops.clone();
        let was_destroyed = destroyed.clone();
        std::thread::spawn(move || {
            // Register this destructor before the timer's thread-local storage.
            HOLDER.with(|holder| holder.borrow_mut().take());
            ntex::rt::System::build()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let ordinary = TimerHandle::new(10_000);
                    let capture = OnDropFn::new(move || {
                        was_destroyed.store(TIMER.try_with(|_| ()).is_err(), Ordering::Relaxed);
                        drop(ordinary);
                        count.fetch_add(1, Ordering::Relaxed);
                    });
                    let callback = CallbackTimer::new(Millis(10_000), move || {
                        let _capture = &capture;
                        panic!("callback after shutdown");
                    });
                    HOLDER.with(|holder| *holder.borrow_mut() = Some(callback));
                });
        })
        .join()
        .unwrap();
        assert!(destroyed.load(Ordering::Relaxed));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
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
            callback: false,
            task: None,
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
        timer.lowres_stime.set(Some(SystemTime::now()));
        timer.insert_flags(
            Flags::RUNNING | Flags::DRIVER_STARTED | Flags::LOWRES_DRIVER | Flags::LOWRES_TIMER,
        );

        drop(LowresTimerDriver {
            timer: timer.clone(),
            generation: 0,
            sleep: Delay::new(Duration::ZERO),
        });
        assert_eq!(timer.flags.get(), Flags::DRIVER_STARTED);
        assert!(timer.lowres_stime.get().is_none());
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

    /// `now()` does not go stale while the wheel is idle.
    #[ntex::test]
    async fn test_driver_cached_time_expires() {
        // the lowres timer of the cache populated at start fires first
        sleep(Millis(400)).await;

        // the wheel is idle, the thread waits for a non-timer event
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            let _ = tx.send_blocking(());
        });
        rx.recv().await.unwrap();

        let stale = Instant::now().saturating_duration_since(now());
        assert!(stale < Duration::from_millis(300), "stale: {stale:?}");
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
