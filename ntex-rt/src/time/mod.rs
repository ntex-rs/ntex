//! Hierarchical timer wheel backing ntex timers.
//!
//! [`TimerHandle`] and [`CallbackHandle`] register timers with the wheel of
//! the current thread, the timer futures of `ntex-util` are built on top of
//! them.
//!
//! The design follows the Linux kernel timer wheel (`kernel/time/timer.c`).
//!
//! # Clock
//!
//! The wheel counts time in *units* of `1 << units` milliseconds. The unit
//! is 16 milliseconds by default, see
//! [`Builder::timer_resolution`](crate::Builder::timer_resolution). Each
//! thread reads the resolution of its system when the first timer is armed
//! and keeps it until the wheel is stopped. The wheel clock `elapsed` is the
//! unit of the last processed expiry, and `elapsed_time` is the [`Instant`]
//! that corresponds to it. Timer deadlines are converted to units relative to
//! this pair.
//!
//! # Levels
//!
//! The wheel has `LVL_DEPTH` (8) levels of `LVL_SIZE` (64) buckets. Each level
//! is 8 times coarser than the previous one, a timer is placed on the first
//! level that can represent its delay. With the default 16 ms unit:
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
//! A smaller unit scales the granularity and the range of every level by the
//! same factor, with a 2 ms unit level 0 covers ~126 ms and the wheel ~3 d.
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
//! # Native runtime
//!
//! Each native [`Runtime`] owns a wheel. `Runtime::poll`
//! expires due buckets before running tasks and reports the time until the
//! next occupied bucket in `PollResult::Pending`, so the driver waits for
//! I/O and timers in a single call. No driver tasks are spawned. The cached
//! [`system_time()`] is refreshed when it is older than 150 milliseconds.
//! The resolution is read from the current system when the first timer is
//! armed, or set with `RuntimeBuilder::timer_resolution`.
//!
//! Handles carry the id of their wheel. A handle used outside the runtime
//! that created it, or after that runtime is dropped, is stale: it reports
//! elapsed, and reset and drop do nothing. Dropping the runtime stops its
//! wheel and wakes all pending timers.
//!
//! # Drivers
//!
//! With the `tokio` and `compio` runtimes the wheel is per thread and two
//! tasks are spawned lazily on the current thread:
//!
//! * `TimerDriver` sleeps until the next occupied bucket expires and wakes
//!   the timers stored in it. The clock advances to the scheduled time of the
//!   bucket, not to the time the driver woke up, so a late wakeup does not
//!   delay later timers. Buckets that are overdue are processed at once.
//!   Wakers are invoked in place while the wheel is held; timer operations
//!   are not reentrant. Callback timers are queued separately and invoked
//!   after releasing the wheel.
//! * `LowresTimerDriver` invalidates the cached [`system_time()`] value
//!   every 150 milliseconds.
//!
//! The driver tasks sleep with the Tokio timer on the `tokio` runtime, so the
//! Tokio runtime must have timers enabled, and with `futures-timer` on
//! `compio`.
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
//! Registrations retain owned [`crate::LocalWaker`] handles, specializing
//! native owner-thread wakes without allocating wrappers or growing entries.
//! Standard wakers are still accepted at polling boundaries.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};
use std::{cell::Cell, cmp, fmt, mem, rc::Rc, task, task::Poll};
use std::{marker::PhantomData, num::NonZeroU64};

use slab::Slab;

use crate::rt::Runtime;

mod callbacks;
mod driver;

pub use self::callbacks::CallbackHandle;
use self::callbacks::{CallbackTicket, Callbacks};
use self::driver::{LowresTimerDriver, TimerDriver};

/// Default resolution of the wheel clock, a unit is `1 << units` milliseconds.
const DEFAULT_UNITS: u8 = 4;

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

const fn to_units(millis: u64, units: u8) -> u64 {
    millis >> units
}

const fn to_millis(val: u64, units: u8) -> u64 {
    val << units
}

/// Resolution of the current system, `DEFAULT_UNITS` without a system.
fn system_units() -> u8 {
    crate::System::try_current().map_or(DEFAULT_UNITS, |sys| {
        sys.timer_resolution().trailing_zeros() as u8
    })
}

const fn as_millis(dur: Duration) -> u64 {
    dur.as_secs() * 1_000 + (dur.subsec_millis() as u64)
}

/// Returns a cached approximation of the current system time.
///
/// The cached value is refreshed at roughly 150 millisecond intervals.
#[inline]
pub fn system_time() -> SystemTime {
    if Runtime::has_current() {
        Runtime::with_current(|rt| rt.timer.native_system_time(rt.now()))
    } else {
        TIMER.with(Timer::system_time)
    }
}

/// Calls `f` with the wheel of the current native runtime, or with the
/// thread-local wheel outside of a native runtime.
fn with_timer<R>(f: impl FnOnce(&Rc<Timer>) -> R) -> R {
    if Runtime::has_current() {
        Runtime::with_current(|rt| f(&rt.timer))
    } else {
        TIMER.with(f)
    }
}

/// Calls `f` with the wheel that issued `hnd` and the slot of the handle.
///
/// Returns `None` for a stale handle, whose wheel is not reachable from the
/// current context, for example after its runtime has been dropped.
fn with_slot<R>(hnd: NonZeroU64, f: impl FnOnce(&Rc<Timer>, usize) -> R) -> Option<R> {
    let mut f = Some(f);
    if Runtime::has_current()
        && let Some(res) = Runtime::with_current(|rt| {
            rt.timer
                .slot(hnd)
                .map(|slot| (f.take().unwrap())(&rt.timer, slot))
        })
    {
        return Some(res);
    }
    // the wheel is already destroyed if the handle is dropped by another
    // thread-local destructor
    TIMER
        .try_with(|t| t.slot(hnd).map(|slot| (f.take().unwrap())(t, slot)))
        .ok()
        .flatten()
}

#[derive(Debug)]
/// Handle to a timer registered with ntex's local timer wheel.
///
/// Dropping the handle cancels the timer. A handle may be reset and reused
/// after it has elapsed.
///
/// Timer operations are not reentrant. Registered wakers must not synchronously
/// access the timer wheel when cloned, woken or dropped.
///
/// The handle refers to a slot in the current thread's wheel, so it is neither
/// `Send` nor `Sync`, and neither are the timer futures that hold it. To run a
/// timer on another thread, send a future that creates it there instead of
/// one that already holds it.
///
/// Inside a native [`Runtime`] the timer is registered
/// with the wheel of that runtime. Once the runtime is dropped, or while it
/// is not active on the thread, the handle is stale: it reports the timer as
/// elapsed and ignores other operations.
pub struct TimerHandle(NonZeroU64, PhantomData<*const ()>);

impl TimerHandle {
    /// Registers a timer that elapses after `millis`.
    pub fn new(millis: u64) -> Self {
        with_timer(|t| t.add_timer(millis))
    }

    /// Restarts the timer with a new delay in milliseconds.
    pub fn reset(&self, millis: u64) {
        with_slot(self.0, |t, slot| t.update_timer(slot, millis));
    }

    /// Completes the timer immediately and wakes its registered task.
    pub fn elapse(&self) {
        with_slot(self.0, |t, slot| t.remove_timer(slot));
    }

    /// Returns `true` if this timer has elapsed.
    pub fn is_elapsed(&self) -> bool {
        with_slot(self.0, |t, slot| {
            t.with_wheel(|w| w.timers[slot].bucket.is_none())
        })
        .unwrap_or(true)
    }

    /// Polls until this timer has elapsed.
    ///
    /// The timer stores one waker: a pending poll replaces the previously
    /// registered one, so only the most recent waiter is woken.
    pub fn poll_elapsed(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        with_slot(self.0, |t, slot| {
            t.with_wheel(|w| {
                let entry = &mut w.timers[slot];
                if entry.bucket.is_none() {
                    Poll::Ready(())
                } else {
                    if entry.task.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                        entry.task = Some(crate::LocalWaker::from_waker(cx.waker()));
                    }
                    Poll::Pending
                }
            })
        })
        .unwrap_or(Poll::Ready(()))
    }
}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        with_slot(self.0, |t, slot| {
            t.with_wheel(|w| {
                w.unlink(slot);
                w.timers.remove(slot);
            });
        });
    }
}

/// Waker slot of a driver task.
#[derive(Default)]
struct DriverWaker(Cell<Option<crate::LocalWaker>>);

impl DriverWaker {
    fn register(&self, waker: &task::Waker) {
        let prev = self.0.take();
        let same = prev.as_ref().is_some_and(|prev| prev.will_wake(waker));
        self.0.set(prev);
        if !same {
            // Keep the old registration until cloning succeeds, then publish
            // before dropping it, waker callbacks may reenter the slot.
            let next = crate::LocalWaker::from_waker(waker);
            drop(self.0.replace(Some(next)));
        }
    }

    fn wake(&self) {
        if let Some(waker) = self.0.take() {
            waker.wake();
        }
    }

    fn take_local(&self) -> Option<crate::LocalWaker> {
        self.0.take()
    }

    #[cfg(test)]
    fn is_set(&self) -> bool {
        let waker = self.0.take();
        let set = waker.is_some();
        self.0.set(waker);
        set
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
        /// The wheel belongs to a native runtime, which expires its timers
        /// in `Runtime::poll`, no driver tasks are spawned.
        const NATIVE         = 0b0100_0000;
    }
}

/// Last issued wheel id, ids identify the wheel that issued a handle.
static NEXT_ID: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static TIMER: Rc<Timer> = Rc::new(Timer::new(None));
}

/// Timer state shared by the timer handles and the driver tasks, per thread
/// or per native runtime.
pub(crate) struct Timer {
    /// Wheel id, the upper half of every handle.
    id: u32,
    /// Resolution of the wheel clock, overrides the system resolution.
    resolution: Option<u8>,
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
    /// Resolution of the wheel clock, a unit is `1 << units` milliseconds.
    /// Read from the current system when the first timer is armed, `None`
    /// while the wheel is stopped.
    units: Cell<Option<u8>>,
    driver: DriverWaker,
    lowres_stime: Cell<Option<SystemTime>>,
    /// Instant the cached time of a native runtime was taken at.
    lowres_stamp: Cell<Option<Instant>>,
    lowres_driver: DriverWaker,
    /// Taken out of the cell for the duration of an operation.
    wheel: Cell<Option<Box<Wheel>>>,
}

impl fmt::Debug for Timer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timer")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Outcome of expiring the due buckets.
pub(super) enum Processed {
    /// The wheel is empty.
    Idle,
    /// The next bucket is due at the instant.
    Next(Instant),
    /// A callback stopped the wheel generation that was processed.
    Stopped,
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
    /// Allocated only for callbacks, keeping ordinary entries waker-sized.
    callbacks: Option<Box<Callbacks>>,
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
struct TimerEntry {
    /// Bucket index, `None` once the timer has elapsed.
    bucket: Option<u16>,
    /// Key of the entry in the bucket.
    bucket_entry: usize,
    callback: bool,
    task: Option<crate::LocalWaker>,
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
    /// Creates a wheel with a new id, `resolution` overrides the system
    /// resolution, in units.
    fn new(resolution: Option<u8>) -> Self {
        let mut timers = Slab::default();
        timers.insert(TimerEntry {
            bucket: None,
            bucket_entry: 0,
            callback: false,
            task: None,
        });

        // ids are never zero, so handles are non-zero
        let id = NEXT_ID
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .max(1);

        Timer {
            id,
            resolution,
            elapsed: Cell::new(0),
            elapsed_time: Cell::new(None),
            next_expiry: Cell::new(u64::MAX),
            flags: Cell::new(Flags::empty()),
            generation: Cell::new(0),
            units: Cell::new(None),
            driver: DriverWaker::default(),
            lowres_stime: Cell::new(None),
            lowres_stamp: Cell::new(None),
            lowres_driver: DriverWaker::default(),
            wheel: Cell::new(Some(Box::new(Wheel {
                timers,
                buckets: (0..WHEEL_SIZE).map(|_| Slab::new()).collect(),
                occupied: [0; LVL_DEPTH as usize],
                callbacks: None,
            }))),
        }
    }

    /// Creates the wheel of a native runtime, `resolution_ms` overrides the
    /// resolution of the system.
    pub(crate) fn new_native(resolution_ms: Option<u8>) -> Rc<Self> {
        let timer = Timer::new(resolution_ms.map(|ms| ms.trailing_zeros() as u8));
        timer.insert_flags(Flags::NATIVE);
        Rc::new(timer)
    }

    /// Returns the handle of `slot`.
    fn handle(&self, slot: usize) -> NonZeroU64 {
        let slot = u32::try_from(slot).expect("timer slots are exhausted");
        NonZeroU64::new((u64::from(self.id) << 32) | u64::from(slot)).unwrap()
    }

    /// Returns the slot of `hnd`, `None` if another wheel issued it.
    fn slot(&self, hnd: NonZeroU64) -> Option<usize> {
        let hnd = hnd.get();
        if (hnd >> 32) as u32 == self.id {
            Some((hnd & u64::from(u32::MAX)) as usize)
        } else {
            None
        }
    }

    fn with_wheel<F, R>(&self, f: F) -> R
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

    /// Returns the cached system time of a native runtime, refreshed once it
    /// is `LOWRES_RESOLUTION` older than `now`.
    pub(crate) fn native_system_time(&self, now: Instant) -> SystemTime {
        if let (Some(cur), Some(stamp)) = (self.lowres_stime.get(), self.lowres_stamp.get())
            && now.saturating_duration_since(stamp) < LOWRES_RESOLUTION
        {
            cur
        } else {
            let cur = SystemTime::now();
            self.lowres_stime.set(Some(cur));
            self.lowres_stamp.set(Some(now));
            cur
        }
    }

    /// Expires the due buckets of a native runtime and runs their callbacks.
    pub(crate) fn run_due(&self) {
        if self.next_expiry.get() != u64::MAX {
            // generation changes only when the runtime is dropped
            let _ = self.process(Instant::now(), self.generation.get());
        }
    }

    /// Returns the instant the next bucket is due, `None` if the wheel is empty.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let expiry = self.next_expiry.get();
        if expiry == u64::MAX {
            None
        } else {
            Some(self.expiry_time(expiry))
        }
    }

    /// Elapses all timers without waking their tasks, called when the native
    /// runtime is dropped.
    pub(crate) fn shutdown(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.stop_wheel();
    }

    /// Expires the buckets that are due at `now` and runs their callbacks.
    ///
    /// Returns `Stopped` if a callback stopped `generation`.
    pub(super) fn process(&self, mut now: Instant, generation: u64) -> Processed {
        loop {
            let expiry = self.next_expiry.get();
            if expiry == u64::MAX {
                return Processed::Idle;
            }

            let deadline = self.expiry_time(expiry);
            if deadline > now {
                return Processed::Next(deadline);
            }

            if let Some(callbacks) = self.expire(expiry, deadline) {
                if !self.run_callbacks(callbacks, generation) {
                    return Processed::Stopped;
                }
                now = Instant::now();
            }
        }
    }

    /// Expires the bucket due at `expiry`, waking its tasks.
    ///
    /// Returns the callbacks that are ready to run.
    fn expire(&self, expiry: u64, deadline: Instant) -> Option<Vec<CallbackTicket>> {
        // Advance the clock to the scheduled time of the bucket, a late
        // wakeup must not shift the timers that expire later
        self.elapsed.set(expiry);
        self.elapsed_time.set(Some(deadline));

        let (next, callbacks) = self.with_wheel(|w| {
            w.execute_expired_timers(expiry);
            let callbacks = w.callbacks.as_mut().and_then(|callbacks| {
                if callbacks.ready.is_empty() {
                    None
                } else {
                    Some(mem::take(&mut callbacks.ready))
                }
            });
            (self.next_pending_bucket(w), callbacks)
        });

        if let Some(next) = next {
            self.next_expiry.set(next);
        } else {
            self.next_expiry.set(u64::MAX);
            self.elapsed_time.set(None);
        }
        callbacks
    }

    /// Runs the ready callbacks, returns `false` if a callback stopped
    /// `generation`.
    fn run_callbacks(&self, mut callbacks: Vec<CallbackTicket>, generation: u64) -> bool {
        for ticket in callbacks.drain(..) {
            if self.generation.get() != generation {
                return false;
            }
            if ticket.generation == generation
                && let Some(state) = ticket.state.upgrade()
                && state.arm.get() == ticket.arm
                && state.pending.replace(false)
            {
                (state.callback)();
            }
        }
        // A callback or its captures may have stopped this runtime.
        if self.generation.get() != generation {
            return false;
        }
        // keep the allocation for the next batch
        self.with_wheel(|wheel| wheel.callbacks.as_mut().unwrap().ready = callbacks);
        true
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

        TimerHandle(self.handle(no), PhantomData)
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
    fn calc_bucket(self: &Rc<Self>, millis: u64) -> (usize, u64) {
        self.insert_flags(Flags::RUNNING);

        // The delay is measured from the wheel clock. The cached time is not
        // used, it goes stale while the thread is blocked and the timer would
        // fire early by its age
        let units = if let Some(units) = self.units.get() {
            units
        } else {
            // the wheel is stopped, it is empty and its clock is at zero
            let units = self.resolution.unwrap_or_else(system_units);
            self.units.set(Some(units));
            units
        };

        let since = Instant::now().saturating_duration_since(self.elapsed_time());
        let delta = to_units(as_millis(since) + millis, units);
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
    ///
    /// A native runtime reads the deadline when its poll finishes.
    fn update_next_expiry(self: &Rc<Self>, expiry: u64) {
        if expiry < self.next_expiry.get() {
            self.next_expiry.set(expiry);
            let flags = self.flags.get();
            // a native runtime expires its timers in `Runtime::poll`
            if flags.contains(Flags::NATIVE) {
                return;
            }
            if flags.contains(Flags::DRIVER_STARTED) {
                self.driver.wake();
            } else {
                TimerDriver::start(self);
            }
        }
    }

    /// Instant at which the bucket expiring at `expiry` is due.
    fn expiry_time(&self, expiry: u64) -> Instant {
        let units = self.units.get().unwrap_or(DEFAULT_UNITS);
        self.elapsed_time()
            + Duration::from_millis(to_millis(expiry.saturating_sub(self.elapsed.get()), units))
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
            self.units.set(None);
        });
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, RawWaker, RawWakerVTable, Wake, Waker};
    use std::{future::Future, pin::Pin};

    use super::driver::Sleep;

    /// Returns the wheel slot of `hnd`.
    fn slot(hnd: NonZeroU64) -> usize {
        (hnd.get() & u64::from(u32::MAX)) as usize
    }

    use super::*;
    use crate::testing::TestRunner;

    /// Completes after `millis`, like the `Sleep` future of `ntex-util`.
    fn sleep(millis: u64) -> impl Future<Output = ()> {
        let hnd = TimerHandle::new(cmp::max(millis, 1));
        std::future::poll_fn(move |cx| hnd.poll_elapsed(cx))
    }

    fn block_on(fut: impl Future<Output = ()> + 'static) {
        crate::System::build().build(TestRunner).block_on(fut);
    }

    struct OnDropFn<F: FnOnce()>(Option<F>);

    impl<F: FnOnce()> OnDropFn<F> {
        fn new(f: F) -> Self {
            Self(Some(f))
        }
    }

    impl<F: FnOnce()> Drop for OnDropFn<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }

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

    /// STATEMENT: Callback timer diagnostics expose the handle without borrowing the wheel or invoking callbacks.
    ///
    /// HOW:
    /// - Format an armed timer while the wheel is borrowed, then cancel and format it again.
    /// - Check the same handle is reported and the captured callback has not run.
    #[test]
    fn debug_callback_timer() {
        let timer = timer_state();
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let callback = CallbackHandle::new(10_000, move || {
            captured.set(captured.get() + 1);
        });
        let expected = format!("CallbackHandle {{ handle: {}, .. }}", callback.hnd);
        timer.with_wheel(|_| assert_eq!(format!("{callback:?}"), expected));
        assert!(!callback.is_elapsed());
        callback.cancel();
        assert_eq!(format!("{callback:?}"), expected);
        assert!(callback.is_elapsed());
        assert_eq!(calls.get(), 0);
    }

    /// STATEMENT: `Timer` diagnostics report the wheel id without borrowing the wheel.
    ///
    /// HOW:
    /// - Format the timer while the wheel is taken out by `with_wheel`.
    /// - Check the output names the wheel id that forms the upper half of a timer handle.
    #[test]
    fn debug_timer() {
        let timer = timer_state();
        let callback = CallbackHandle::new(10_000, || {});
        let expected = format!("Timer {{ id: {}, .. }}", callback.hnd.get() >> 32);
        timer.with_wheel(|_| assert_eq!(format!("{timer:?}"), expected));
        assert_eq!(format!("{timer:?}"), expected);
    }

    fn timer_driver(timer: &Rc<Timer>) -> TimerDriver {
        TimerDriver {
            timer: timer.clone(),
            generation: timer.generation.get(),
            sleep: Sleep::new(Instant::now()),
            armed: None,
        }
    }

    /// Polls `driver` once with a no-op waker.
    ///
    /// With the `tokio` or `compio` feature the driver sleep needs a runtime
    /// context; the runtime is not driven, so the sleep never fires here.
    fn poll_driver(driver: &mut TimerDriver) -> Poll<()> {
        let poll = || Pin::new(driver).poll(&mut Context::from_waker(Waker::noop()));

        #[cfg(feature = "tokio")]
        {
            static RT: std::sync::LazyLock<tok_io::runtime::Runtime> =
                std::sync::LazyLock::new(|| {
                    tok_io::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .unwrap()
                });
            let _guard = RT.enter();
            poll()
        }
        #[cfg(all(feature = "compio", not(feature = "tokio")))]
        {
            thread_local! {
                static RT: compio_runtime::Runtime = compio_runtime::Runtime::new().unwrap();
            }
            RT.with(|rt| rt.enter(poll))
        }
        #[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
        poll()
    }

    fn make_due(timer: &Timer) {
        timer.elapsed_time.set(Some(
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        ));
    }

    #[derive(Debug)]
    struct NoopNotify;

    impl crate::Notify for NoopNotify {
        fn notify(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// STATEMENT: Timer waker registration avoids redundant clones and survives replacement panics.
    ///
    /// HOW:
    /// - Count raw-waker clones and references while re-registering, resetting, and replacing.
    /// - Catch a clone panic, then verify expiry wakes only the replacement and drop releases it.
    #[test]
    fn test_local_waker_registration() {
        struct Registration {
            inner: Waker,
            clones: AtomicUsize,
            panic_on_clone: AtomicBool,
        }

        // Keep clone identity stable without relying on generic vtable deduplication.
        static VTABLE: RawWakerVTable = RawWakerVTable::new(
            |data| {
                // SAFETY: Each waker holds a live `Arc<Registration>` count; cloning adds one for the new waker.
                unsafe {
                    let state = &*data.cast::<Registration>();
                    assert!(!state.panic_on_clone.load(Ordering::Relaxed), "clone");
                    state.clones.fetch_add(1, Ordering::Relaxed);
                    Arc::increment_strong_count(data.cast::<Registration>());
                    RawWaker::new(data, &VTABLE)
                }
            },
            |data| {
                // SAFETY: Consuming this waker transfers its one raw `Arc` count into the temporary owner.
                unsafe {
                    Arc::from_raw(data.cast::<Registration>())
                        .inner
                        .wake_by_ref();
                }
            },
            |data| {
                // SAFETY: The borrowed waker keeps the `Arc<Registration>` allocation live for this call.
                unsafe {
                    (*data.cast::<Registration>()).inner.wake_by_ref();
                }
            },
            |data| {
                // SAFETY: Dropping this waker releases exactly the raw `Arc` count it owns.
                unsafe {
                    drop(Arc::from_raw(data.cast::<Registration>()));
                }
            },
        );

        let wrap = |task: &Arc<InPlaceWaker>| {
            let state = Arc::new(Registration {
                inner: Waker::from(task.clone()),
                clones: AtomicUsize::new(0),
                panic_on_clone: AtomicBool::new(false),
            });
            // SAFETY: The vtable manages the transferred `Arc` count and accesses only thread-safe state.
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
                wheel.timers[slot(handle.0)]
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
            wheel.relink(slot(handle.0), 3);
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

    /// STATEMENT: A timer wakes the most recently registered native task and supports later reuse.
    ///
    /// HOW:
    /// - Register two runtime tasks on one handle and expire it, checking only the second finishes.
    /// - Rearm for the first task, expire again, and wake its saved waker after runtime shutdown.
    #[test]
    fn test_local_waker_native_replacement() {
        let timer = timer_state();
        let rt = crate::Runtime::new(Box::new(NoopNotify));
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
            wheel.relink(slot(handle.0), 3);
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

    /// STATEMENT: Timer handles safely retain stale task wakers across runtime shutdown and reuse.
    ///
    /// HOW:
    /// - Register timer and driver wakers, drop the runtime, and reset timer state.
    /// - Check released task references and driver slots, then optionally reuse the handle in a new runtime.
    #[test]
    fn test_local_waker_runtime_shutdown() {
        for reuse in [false, true] {
            let timer = timer_state();
            let rt = crate::Runtime::new(Box::new(NoopNotify));
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
                assert!(wheel.timers[slot(handle.0)].task.is_some());
            });

            if reuse {
                timer.insert_flags(Flags::DRIVER_STARTED);
                handle.reset(10_000);
                let rt = crate::Runtime::new(Box::new(NoopNotify));
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

    /// STATEMENT: Timer wakers can be replaced across successive systems on the same thread.
    ///
    /// HOW:
    /// - Reuse one handle across three systems with alternating native and wrapped wakers.
    /// - Reset and expire it in each system and check wrapped notification counts after shutdown.
    // a native runtime owns its wheel, handles do not outlive it
    #[cfg(any(feature = "tokio", feature = "compio"))]
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
                    crate::System::build()
                        .build(TestRunner)
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

    /// STATEMENT: Callback timers can be canceled and repeatedly rearmed for one delivery each.
    ///
    /// HOW:
    /// - Cancel a reset timer and verify no callback runs after waiting.
    /// - Rearm with zero delay twice and check elapsed state and one additional call per expiry.
    #[test]
    fn test_callback_timer() {
        block_on(async {
            let calls = Rc::new(Cell::new(0));
            let count = calls.clone();
            let callback = CallbackHandle::new(0, move || {
                count.set(count.get() + 1);
            });
            assert!(!callback.is_elapsed());
            assert_eq!(calls.get(), 0);
            callback.reset(10_000);
            callback.cancel();
            assert!(callback.is_elapsed());
            sleep(50).await;
            assert_eq!(calls.get(), 0);

            for expected in 1..=2 {
                callback.reset(0);
                assert!(!callback.is_elapsed());
                sleep(50).await;
                assert!(callback.is_elapsed());
                assert_eq!(calls.get(), expected);
            }
        });
    }

    /// STATEMENT: Stale callback tickets cannot invoke canceled, reset, dropped, or reused entries.
    ///
    /// HOW:
    /// - Queue expired callbacks, then cancel, reset, and drop them before driver delivery.
    /// - Reuse the freed slot, verify captures were released, and check only new deadlines fire once.
    #[test]
    fn test_callback_stale_tickets() {
        let timer = timer_state();
        let calls = Rc::new(Cell::new(0));
        let count = calls.clone();
        let cancelled = CallbackHandle::new(1, move || count.set(count.get() + 1));
        let count = calls.clone();
        let reset = CallbackHandle::new(1, move || count.set(count.get() + 1));
        let capture = Rc::new(Cell::new(0));
        let weak = Rc::downgrade(&capture);
        let dropped = CallbackHandle::new(1, move || capture.set(1));
        let old_slot = dropped.hnd;
        timer.with_wheel(|wheel| {
            for handle in [&cancelled, &reset, &dropped] {
                wheel.relink(slot(handle.hnd), 3);
            }
            wheel.execute_expired_timers(3);
        });
        assert!(cancelled.is_elapsed());
        assert!(reset.is_elapsed());
        assert!(dropped.is_elapsed());
        assert_eq!(calls.get(), 0);
        cancelled.cancel();
        reset.reset(10_000);
        drop(dropped);
        assert!(
            weak.upgrade().is_none(),
            "queued tickets must not retain captures"
        );
        let count = calls.clone();
        let replacement = CallbackHandle::new(10_000, move || count.set(count.get() + 100));
        assert_eq!(replacement.hnd, old_slot);

        make_due(&timer);
        let mut driver = timer_driver(&timer);
        assert!(poll_driver(&mut driver).is_pending());
        assert_eq!(calls.get(), 0);
        assert!(!reset.is_elapsed());
        assert!(!replacement.is_elapsed());
        timer.elapsed_time.set(Some(
            Instant::now().checked_sub(Duration::from_secs(20)).unwrap(),
        ));
        for _ in 0..2 {
            assert!(poll_driver(&mut driver).is_pending());
            assert_eq!(calls.get(), 101);
        }
    }

    /// STATEMENT: A callback can rearm itself and cancel another callback already due for delivery.
    ///
    /// HOW:
    /// - Expire two callbacks that each cancel their peer and reset themselves.
    /// - Verify only one runs initially and its rearmed deadline produces one further call.
    #[test]
    fn test_callback_self_rearm_and_cross_cancel() {
        let timer = timer_state();
        let calls = Rc::new(Cell::new(0));
        let handles = Rc::new(RefCell::new(Vec::<CallbackHandle>::new()));
        for index in 0..2 {
            let count = calls.clone();
            let weak = Rc::downgrade(&handles);
            let callback = CallbackHandle::new(1, move || {
                count.set(count.get() + 1);
                let handles = weak.upgrade().unwrap();
                let handles = handles.borrow();
                assert!(handles[index].is_elapsed());
                handles[1 - index].cancel();
                handles[index].reset(10_000);
            });
            handles.borrow_mut().push(callback);
        }
        timer.with_wheel(|wheel| {
            for handle in handles.borrow().iter() {
                wheel.relink(slot(handle.hnd), 1);
            }
        });
        make_due(&timer);
        let mut driver = timer_driver(&timer);
        assert!(poll_driver(&mut driver).is_pending());
        assert_eq!(calls.get(), 1);
        assert_eq!(
            handles.borrow().iter().filter(|h| !h.is_elapsed()).count(),
            1
        );
        timer.elapsed_time.set(Some(
            Instant::now().checked_sub(Duration::from_secs(20)).unwrap(),
        ));
        for _ in 0..2 {
            assert!(poll_driver(&mut driver).is_pending());
            assert_eq!(calls.get(), 2);
        }
    }

    /// STATEMENT: Dropping callback captures can safely create, reset, and cancel timers.
    ///
    /// HOW:
    /// - Drop queued and actively executing callbacks whose capture destructors manipulate timers.
    /// - Check deferred capture release, slot reuse, cancellation, and exactly-once later delivery.
    #[test]
    fn test_callback_capture_drop_schedules_timers() {
        for active in [false, true] {
            let timer = timer_state();
            let released = Rc::new(Cell::new(false));
            let finished = Rc::new(Cell::new(false));
            let reset_calls = Rc::new(Cell::new(0));
            let new_calls = Rc::new(Cell::new(0));
            let count = reset_calls.clone();
            let reset = Rc::new(CallbackHandle::new(20_000, move || {
                count.set(count.get() + 1);
            }));
            let cancelled = Rc::new(CallbackHandle::new(20_000, || {
                panic!("cancelled callback");
            }));
            let replacement = Rc::new(RefCell::new(None::<CallbackHandle>));
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
                    reset.reset(10_000);
                    *replacement.borrow_mut() = Some(CallbackHandle::new(10_000, move || {
                        new_calls.set(new_calls.get() + 1);
                    }));
                    released.set(true);
                })
            };
            let handle = Rc::new(RefCell::new(None::<CallbackHandle>));
            let weak = Rc::downgrade(&handle);
            let was_released = released.clone();
            *handle.borrow_mut() = Some(CallbackHandle::new(1, move || {
                let _capture = &capture;
                assert!(active, "dropped queued callback must not run");
                drop(weak.upgrade().unwrap().borrow_mut().take());
                assert!(!was_released.get());
                finished.set(true);
            }));
            let old_slot = handle.borrow().as_ref().unwrap().hnd;
            if !active {
                timer.with_wheel(|wheel| {
                    wheel.relink(slot(old_slot), 1);
                    wheel.execute_expired_timers(1);
                });
                drop(handle.borrow_mut().take());
                assert!(released.get());
            }
            make_due(&timer);
            let mut driver = timer_driver(&timer);
            assert!(poll_driver(&mut driver).is_pending());
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
                assert!(poll_driver(&mut driver).is_pending());
                assert_eq!((reset_calls.get(), new_calls.get()), (1, 1));
            }
        }
    }

    /// STATEMENT: A callback capture can reset the runtime without the old driver consuming new work.
    ///
    /// HOW:
    /// - Drop a callback during delivery and reset timer state from its capture destructor.
    /// - Queue a replacement, verify the old driver exits, and deliver it once with a new driver.
    #[test]
    fn test_callback_capture_drop_resets_runtime() {
        let timer = timer_state();
        let new_calls = Rc::new(Cell::new(0));
        let replacement = Rc::new(RefCell::new(None::<CallbackHandle>));
        let capture = {
            let timer = timer.clone();
            let new_calls = new_calls.clone();
            let replacement = replacement.clone();
            OnDropFn::new(move || {
                timer.reset();
                timer.insert_flags(Flags::DRIVER_STARTED);
                let callback = CallbackHandle::new(1, move || {
                    new_calls.set(new_calls.get() + 1);
                });
                timer.with_wheel(|wheel| {
                    wheel.relink(slot(callback.hnd), 1);
                    wheel.execute_expired_timers(1);
                });
                *replacement.borrow_mut() = Some(callback);
            })
        };
        let handle = Rc::new(RefCell::new(None::<CallbackHandle>));
        let weak = Rc::downgrade(&handle);
        *handle.borrow_mut() = Some(CallbackHandle::new(1, move || {
            let _capture = &capture;
            drop(weak.upgrade().unwrap().borrow_mut().take());
        }));
        make_due(&timer);
        let mut old_driver = timer_driver(&timer);
        assert!(poll_driver(&mut old_driver).is_ready());
        assert!(handle.borrow().is_none());
        assert!(replacement.borrow().as_ref().unwrap().is_elapsed());
        assert_eq!(new_calls.get(), 0);
        drop(old_driver);

        make_due(&timer);
        let mut driver = timer_driver(&timer);
        for _ in 0..2 {
            assert!(poll_driver(&mut driver).is_pending());
            assert_eq!(new_calls.get(), 1);
        }
    }

    /// STATEMENT: Callback delivery refreshes time after a slow callback.
    ///
    /// HOW:
    /// - Schedule two deadlines and make the first callback block past the second.
    /// - Poll the driver once and verify both callbacks ran and both timers elapsed.
    #[test]
    fn test_callback_refreshes_time() {
        let timer = timer_state();
        let calls = Rc::new(Cell::new(0));
        let handles = [1, 32].map(|millis| {
            let calls = calls.clone();
            CallbackHandle::new(millis, move || {
                let previous = calls.replace(calls.get() + 1);
                if previous == 0 {
                    std::thread::sleep(Duration::from_millis(64));
                }
            })
        });
        timer.with_wheel(|wheel| {
            wheel.relink(slot(handles[0].hnd), 1);
            wheel.relink(slot(handles[1].hnd), 3);
        });
        timer.elapsed_time.set(Some(
            Instant::now()
                .checked_sub(Duration::from_millis(32))
                .unwrap(),
        ));
        let mut driver = timer_driver(&timer);
        assert!(poll_driver(&mut driver).is_pending());
        assert_eq!(calls.get(), 2);
        assert!(handles.iter().all(CallbackHandle::is_elapsed));
    }

    /// STATEMENT: Dropping a panicked driver cleans callbacks rearmed before the panic.
    ///
    /// HOW:
    /// - Run a callback that rearms itself and panics while other timers remain pending.
    /// - Drop the driver and verify elapsed handles, cleared callback flags, and empty wheel queues.
    #[test]
    fn test_callback_panic_after_rearm_cleanup() {
        let timer = timer_state();
        let callback = Rc::new_cyclic(|weak: &std::rc::Weak<CallbackHandle>| {
            let weak = weak.clone();
            CallbackHandle::new(1, move || {
                let callback = weak.upgrade().unwrap();
                callback.reset(10_000);
                panic!("callback");
            })
        });
        let later = CallbackHandle::new(10_000, || panic!("not due"));
        let ordinary = TimerHandle::new(10_000);
        make_due(&timer);
        let mut driver = timer_driver(&timer);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = poll_driver(&mut driver);
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

    /// STATEMENT: Resetting timer state during callback delivery stops the old generation safely.
    ///
    /// HOW:
    /// - Queue two callbacks that reset runtime state and create a replacement.
    /// - Verify only one old callback runs and the replacement survives old-driver drop for later delivery.
    #[test]
    fn test_callback_runtime_reset_during_delivery() {
        let timer = timer_state();
        let calls = Rc::new(Cell::new(0));
        let new_calls = Rc::new(Cell::new(0));
        let replacement = Rc::new(RefCell::new(None::<CallbackHandle>));
        let handles = [0, 1].map(|_| {
            let timer = timer.clone();
            let calls = calls.clone();
            let new_calls = new_calls.clone();
            let replacement = replacement.clone();
            CallbackHandle::new(1, move || {
                calls.set(calls.get() + 1);
                timer.reset();
                timer.insert_flags(Flags::DRIVER_STARTED);
                let new_calls = new_calls.clone();
                *replacement.borrow_mut() = Some(CallbackHandle::new(1, move || {
                    new_calls.set(new_calls.get() + 1);
                }));
            })
        });
        timer.with_wheel(|wheel| {
            for handle in &handles {
                wheel.relink(slot(handle.hnd), 1);
            }
        });
        make_due(&timer);
        let mut old_driver = timer_driver(&timer);
        assert!(poll_driver(&mut old_driver).is_ready());
        assert_eq!(calls.get(), 1);
        assert!(handles.iter().all(CallbackHandle::is_elapsed));
        drop(old_driver);
        assert!(!replacement.borrow().as_ref().unwrap().is_elapsed());
        make_due(&timer);
        let mut driver = timer_driver(&timer);
        assert!(poll_driver(&mut driver).is_pending());
        assert_eq!(new_calls.get(), 1);
    }

    /// STATEMENT: A waker panic in a mixed timer bucket leaves callback state recoverable.
    ///
    /// HOW:
    /// - Place a panicking waker at each position among two callback entries and catch driver unwind.
    /// - Check bucket and pending-ticket cleanup, then rearm a callback and verify one delivery.
    #[test]
    fn test_callback_mixed_bucket_waker_panic() {
        for panic_position in 0..3 {
            let timer = timer_state();
            let calls = Rc::new(Cell::new(0));
            let callbacks = [0, 1].map(|_| {
                let calls = calls.clone();
                CallbackHandle::new(10_000, move || calls.set(calls.get() + 1))
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
                    slot(callbacks[0].hnd),
                    slot(ordinary.0),
                    slot(callbacks[1].hnd),
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
                    let _ = poll_driver(&mut driver);
                }))
                .is_err()
            );
            assert_eq!(task.wakes.load(Ordering::Relaxed), 1);
            assert_eq!(calls.get(), 0);
            assert!(ordinary.is_elapsed());
            assert!(callbacks.iter().all(CallbackHandle::is_elapsed));
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
            callbacks[0].reset(0);
            make_due(&timer);
            let mut driver = timer_driver(&timer);
            for _ in 0..2 {
                assert!(poll_driver(&mut driver).is_pending());
                assert_eq!(calls.get(), 1);
            }
        }
    }

    /// STATEMENT: A callback timer can be rearmed in a later system without stale delivery.
    ///
    /// HOW:
    /// - Carry one callback handle across two systems on a dedicated thread.
    /// - Verify shutdown elapses it, no callback runs before rearming, and each reset delivers once.
    // a native runtime owns its wheel, handles do not outlive it
    #[cfg(any(feature = "tokio", feature = "compio"))]
    #[test]
    fn test_callback_reuse_in_next_system() {
        std::thread::spawn(|| {
            let calls = Rc::new(Cell::new(0));
            let mut callback = None::<CallbackHandle>;
            for expected in 1..=2 {
                let calls = calls.clone();
                callback = Some(
                    crate::System::build()
                        .build(TestRunner)
                        .block_on(async move {
                            let callback = if let Some(callback) = callback {
                                assert!(callback.is_elapsed());
                                sleep(50).await;
                                assert_eq!(calls.get(), expected - 1);
                                callback.reset(0);
                                callback
                            } else {
                                let calls = calls.clone();
                                CallbackHandle::new(0, move || {
                                    calls.set(calls.get() + 1);
                                })
                            };
                            sleep(50).await;
                            assert_eq!(calls.get(), expected);
                            callback.reset(10_000);
                            callback
                        }),
                );
                assert!(callback.as_ref().unwrap().is_elapsed());
            }
        })
        .join()
        .unwrap();
    }

    /// STATEMENT: Wheel access is restored after an operation panics.
    ///
    /// HOW:
    /// - Insert an entry inside `with_wheel`, panic, and catch the unwind.
    /// - Access the wheel again and verify the inserted entry remains.
    #[test]
    fn test_wheel_access_restored_on_unwind() {
        let timer = Timer::new(None);
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

    /// STATEMENT: Manual and driven expiry wake registered tasks while wheel access is held.
    ///
    /// HOW:
    /// - Register a waker that asserts wheel ownership and expire handles by reset, elapse, and dispatch.
    /// - Check all four wakes, elapsed handles, and released waker references.
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
        assert!(poll_driver(&mut driver).is_pending());
        assert!(handles.iter().all(TimerHandle::is_elapsed));
        assert_eq!(task.wakes.load(Ordering::Relaxed), 4);
        assert_eq!(Arc::strong_count(&task), 2);
    }

    /// STATEMENT: Waker dispatch uses one time sample even if a wake callback blocks.
    ///
    /// HOW:
    /// - Schedule two deadlines and block inside the first wake past the second deadline.
    /// - Verify processing one time sample expires only one timer and a later sample
    ///   expires the other.
    #[test]
    fn test_dispatch_uses_one_time_sample_for_wakers() {
        let timer = timer_state();
        let handles = [TimerHandle::new(1), TimerHandle::new(32)];
        timer.with_wheel(|wheel| wheel.relink(slot(handles[1].0), 3));
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
        let generation = timer.generation.get();
        assert!(matches!(
            timer.process(Instant::now(), generation),
            Processed::Next(_)
        ));
        assert!(handles[0].is_elapsed());
        assert!(!handles[1].is_elapsed());
        assert_eq!(task.wakes.load(Ordering::Relaxed), 1);
        assert!(matches!(
            timer.process(Instant::now(), generation),
            Processed::Idle
        ));
        assert!(handles[1].is_elapsed());
        assert_eq!(task.wakes.load(Ordering::Relaxed), 2);
    }

    /// STATEMENT: A wake panic clears its expired bucket and leaves driver cleanup safe.
    ///
    /// HOW:
    /// - Expire two same-bucket timers with a panicking waker and inspect bucket occupancy.
    /// - Rearm both handles, drop the driver, and verify both elapse and the driver flag clears.
    #[test]
    fn test_dispatch_panic_cleans_expired_bucket() {
        let timer = timer_state();
        let handles = [TimerHandle::new(1), TimerHandle::new(1)];
        timer.with_wheel(|wheel| {
            let bucket = wheel.timers[slot(handles[0].0)].bucket.unwrap();
            wheel.relink(slot(handles[1].0), usize::from(bucket));
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
                let _ = poll_driver(&mut driver);
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

    /// STATEMENT: A timer handle can be dropped after its thread-local wheel is destroyed.
    ///
    /// HOW:
    /// - Register handle storage before timer storage so its destructor runs last.
    /// - Leave a registered handle in that storage and verify the thread exits without panic.
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
            crate::System::build().build(TestRunner).block_on(async {
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

    /// STATEMENT: Callback captures are released once even when their timer outlives wheel storage.
    ///
    /// HOW:
    /// - Store a callback in earlier-registered thread-local storage with a counted drop capture.
    /// - Exit the thread and verify capture destruction occurred after wheel destruction exactly once.
    // a native runtime owns its wheel, handles do not outlive it
    #[cfg(any(feature = "tokio", feature = "compio"))]
    #[test]
    fn test_drop_callback_after_wheel_destroyed() {
        thread_local! {
            static HOLDER: RefCell<Option<CallbackHandle>> = const { RefCell::new(None) };
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let destroyed = Arc::new(AtomicBool::new(false));
        let count = drops.clone();
        let was_destroyed = destroyed.clone();
        std::thread::spawn(move || {
            // Register this destructor before the timer's thread-local storage.
            HOLDER.with(|holder| holder.borrow_mut().take());
            crate::System::build()
                .build(TestRunner)
                .block_on(async move {
                    let ordinary = TimerHandle::new(10_000);
                    let capture = OnDropFn::new(move || {
                        was_destroyed.store(TIMER.try_with(|_| ()).is_err(), Ordering::Relaxed);
                        drop(ordinary);
                        count.fetch_add(1, Ordering::Relaxed);
                    });
                    let callback = CallbackHandle::new(10_000, move || {
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

    /// STATEMENT: Successive systems on one thread start working timers despite prior pending tasks.
    ///
    /// HOW:
    /// - Start three systems, complete a short sleep, and leave long sleeps pending in each.
    /// - Check per-system elapsed bounds and thread completion within an overall deadline.
    #[test]
    fn test_timer_in_next_system() {
        let res = std::thread::spawn(|| {
            for _ in 0..3 {
                let start = Instant::now();
                crate::System::build().build(TestRunner).block_on(async {
                    sleep(10).await;
                    let _ = crate::now();
                    // pending timer of the stopped system
                    let _hnd = sleep(10_000);
                    crate::spawn(async { sleep(10_000).await });
                });
                assert!(start.elapsed() < Duration::from_secs(5));
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(res.join().is_ok()));
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true));
    }

    /// STATEMENT: Each system run on a thread arms timers with its own configured resolution.
    ///
    /// HOW:
    /// - Run systems with 2 ms, default and 1 ms resolutions on one thread.
    /// - In each, complete a sleep and check the latched `units` shift and the
    ///   level of a 500 ms timer, level 1 at 2 ms and level 0 at 16 ms.
    /// - After each run, check `units` is cleared for the next system.
    #[test]
    fn test_timer_resolution_per_system() {
        std::thread::spawn(|| {
            for (resolution, units, level) in [(Some(2), 1, 1), (None, 4, 0), (Some(1), 0, 1)] {
                let mut builder = crate::System::build();
                if let Some(resolution) = resolution {
                    builder = builder.timer_resolution(resolution);
                }
                builder.build(TestRunner).block_on(async move {
                    sleep(1).await;
                    let hnd = TimerHandle::new(500);
                    with_timer(|timer| {
                        assert_eq!(timer.units.get(), Some(units));
                        let bucket = timer.with_wheel(|w| w.timers[slot(hnd.0)].bucket).unwrap();
                        assert_eq!(u64::from(bucket) / LVL_SIZE, level);
                    });
                });
                assert_eq!(TIMER.with(|timer| timer.units.get()), None);
            }
        })
        .join()
        .unwrap();
    }

    /// STATEMENT: Timers armed without a running system use the default 16 ms resolution.
    ///
    /// HOW:
    /// - Reset the thread's timer state and check `units` is unset.
    /// - Arm a timer outside of any system and check `units` is `DEFAULT_UNITS`.
    #[test]
    fn test_timer_resolution_without_system() {
        let timer = timer_state();
        assert_eq!(timer.units.get(), None);
        let _hnd = TimerHandle::new(10);
        assert_eq!(timer.units.get(), Some(DEFAULT_UNITS));
    }

    /// STATEMENT: Dropping stale drivers does not clear a newer generation's running flags.
    ///
    /// HOW:
    /// - Reset timer state, mark the new generation running, and drop both old driver types.
    /// - Verify all new-generation driver flags remain set.
    #[test]
    fn test_stale_driver_drop() {
        let timer = Rc::new(Timer::new(None));
        timer.insert_flags(Flags::RUNNING | Flags::DRIVER_STARTED | Flags::LOWRES_DRIVER);
        timer.reset();
        assert_eq!(timer.flags.get(), Flags::empty());

        timer.insert_flags(Flags::RUNNING | Flags::DRIVER_STARTED | Flags::LOWRES_DRIVER);
        drop(TimerDriver {
            timer: timer.clone(),
            generation: 0,
            sleep: Sleep::new(Instant::now()),
            armed: None,
        });
        drop(LowresTimerDriver {
            timer: timer.clone(),
            generation: 0,
            sleep: Sleep::new(Instant::now()),
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

    /// STATEMENT: Bucket placement never expires early and stays within its level's granularity.
    ///
    /// HOW:
    /// - Calculate buckets for varied elapsed times and exponentially increasing delays.
    /// - Check expiry bounds, alignment, bucket indices, and clamping beyond wheel capacity.
    #[test]
    fn test_bucket_expiry_bounds() {
        let timer = Timer::new(None);
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

    /// STATEMENT: Next-bucket selection visits occupied buckets in increasing expiry order.
    ///
    /// HOW:
    /// - Insert timers with unsorted delays and compute their expected sorted expiries.
    /// - Expire each selected bucket and verify its entry unlinks and the final occupancy is empty.
    #[test]
    fn test_next_pending_bucket() {
        let timer = Timer::new(None);
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

    /// STATEMENT: Linking, unlinking, and relinking keep bucket membership and occupancy consistent.
    ///
    /// HOW:
    /// - Link two entries to one bucket, remove one twice, and move the other to another level.
    /// - Check occupancy bits and membership after each step and after the final removal.
    #[test]
    fn test_unlink_relink() {
        let timer = Timer::new(None);
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

    /// STATEMENT: Resetting a timer to zero promptly wakes its waiting task.
    ///
    /// HOW:
    /// - Reset a shared long timer from another task while selecting against a fallback sleep.
    /// - Require completion well before the fallback deadline.
    #[test]
    fn test_reset_zero_wakes_task() {
        block_on(async {
            let hnd = Rc::new(TimerHandle::new(10_000));
            let hnd2 = hnd.clone();
            crate::spawn(async move { hnd2.reset(0) });

            // the sleep wakes the task if `reset(0)` does not
            let start = Instant::now();
            let mut fallback = std::pin::pin!(sleep(500));
            std::future::poll_fn(|cx| {
                if hnd.poll_elapsed(cx).is_ready() || fallback.as_mut().poll(cx).is_ready() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            let elapsed = start.elapsed();
            assert!(elapsed < Duration::from_millis(100), "elapsed: {elapsed:?}");
        });
    }

    /// STATEMENT: Dropping a timer driver preserves the other driver's state.
    ///
    /// HOW:
    /// - Drop the low-resolution driver and verify regular timer occupancy and flags remain.
    /// - Drop the regular driver and verify its timers clear while the low-resolution flag remains.
    #[test]
    fn test_driver_drop_keeps_other_driver() {
        let timer = Rc::new(Timer::new(None));
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
            sleep: Sleep::new(Instant::now()),
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
            sleep: Sleep::new(Instant::now()),
            armed: None,
        });
        assert_eq!(timer.flags.get(), Flags::LOWRES_DRIVER);
        assert_eq!(timer.next_expiry.get(), u64::MAX);
        timer.with_wheel(|w| {
            assert!(w.timers[no].bucket.is_none());
            assert_eq!(w.occupied, [0; LVL_DEPTH as usize]);
        });
    }

    /// STATEMENT: A new short timer uses current time after the runtime thread was blocked.
    ///
    /// HOW:
    /// - Populate the time cache, block the thread, then start a short sleep.
    /// - Measure with `Instant` and verify the sleep lasts at least its requested duration.
    #[test]
    fn test_short_timer_after_blocking() {
        block_on(async {
            let _hnd = sleep(10_000);
            let _ = crate::now();

            // the lowres driver cannot run, the cached time goes stale
            std::thread::sleep(Duration::from_millis(100));

            let start = Instant::now();
            sleep(50).await;
            let elapsed = start.elapsed();
            assert!(elapsed >= Duration::from_millis(50), "elapsed: {elapsed:?}");
        });
    }

    /// STATEMENT: Cached `crate::now()` expires even when the timer wheel becomes idle.
    ///
    /// HOW:
    /// - Let existing timers finish, then wait for a channel message from a sleeping thread.
    /// - Compare `crate::now()` with the real clock and require less than 300 milliseconds of staleness.
    #[test]
    fn test_driver_cached_time_expires() {
        block_on(async {
            // the lowres timer of the cache populated at start fires first
            sleep(400).await;

            // the wheel is idle, the thread waits for a non-timer event
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(600));
                let _ = tx.send_blocking(());
            });
            rx.recv().await.unwrap();

            let stale = Instant::now().saturating_duration_since(crate::now());
            assert!(stale < Duration::from_millis(300), "stale: {stale:?}");
        });
    }

    /// STATEMENT: A delayed driver wake does not shift later timers by the same delay.
    ///
    /// HOW:
    /// - Start short and long sleeps, then block the thread past the short deadline.
    /// - Await both and check total elapsed time stays near the long timer's original deadline.
    #[test]
    fn test_late_wakeup_does_not_drift() {
        block_on(async {
            let start = Instant::now();
            let fut1 = sleep(50);
            let fut2 = sleep(600);

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
        });
    }

    /// STATEMENT: `TimerHandle` and `CallbackHandle` are neither `Send` nor `Sync`,
    /// so they cannot reach another thread's wheel.
    ///
    /// HOW:
    /// - Define a trait with a blanket impl for all types and a second impl for
    ///   `Send` (or `Sync`) types.
    /// - Resolve the trait item with an inferred parameter for `TimerHandle` and
    ///   `CallbackHandle`; it compiles only if exactly one impl applies, i.e. the
    ///   type is not `Send` (or `Sync`).
    #[test]
    fn timer_handle_is_thread_local() {
        trait NotSend<A> {
            fn check() {}
        }
        impl<T: ?Sized> NotSend<()> for T {}
        impl<T: ?Sized + Send> NotSend<u8> for T {}
        trait NotSync<A> {
            fn check() {}
        }
        impl<T: ?Sized> NotSync<()> for T {}
        impl<T: ?Sized + Sync> NotSync<u8> for T {}

        <TimerHandle as NotSend<_>>::check();
        <TimerHandle as NotSync<_>>::check();
        <CallbackHandle as NotSend<_>>::check();
        <CallbackHandle as NotSync<_>>::check();
    }

    /// STATEMENT: A native runtime caches the system time until it is `LOWRES_RESOLUTION` old.
    ///
    /// HOW:
    /// - Read the time of a native wheel, then read it again just before the cache expires.
    /// - Check both reads match and a read at `LOWRES_RESOLUTION` takes a new time.
    #[test]
    fn test_native_system_time_refresh() {
        let timer = Timer::new_native(None);
        let now = Instant::now();
        let first = timer.native_system_time(now);
        std::thread::sleep(Duration::from_millis(2));
        let almost = now + (LOWRES_RESOLUTION.saturating_sub(Duration::from_millis(1)));
        assert_eq!(timer.native_system_time(almost), first);
        let fresh = timer.native_system_time(now + LOWRES_RESOLUTION);
        assert!(fresh > first);
        assert_eq!(timer.native_system_time(now + LOWRES_RESOLUTION), fresh);
    }
}
