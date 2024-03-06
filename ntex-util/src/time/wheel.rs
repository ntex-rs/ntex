//! Time wheel based timer service.
//!
//! Inspired by linux kernel timers system
#![allow(arithmetic_overflow, clippy::let_underscore_future)]
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant, SystemTime};
use std::{cmp::max, future::Future, mem, pin::Pin, rc::Rc, task, task::Poll};

use futures_timer::Delay;
use slab::Slab;

use crate::task::LocalWaker;

// Clock divisor for the next level
const LVL_CLK_SHIFT: u64 = 3;
const LVL_CLK_DIV: u64 = 1 << LVL_CLK_SHIFT;
const LVL_CLK_MASK: u64 = LVL_CLK_DIV - 1;

const fn lvl_shift(n: u64) -> u64 {
    n * LVL_CLK_SHIFT
}

const fn lvl_gran(n: u64) -> u64 {
    1 << lvl_shift(n)
}

// Resolution:
// 0: 1 millis
// 4: ~17 millis
const UNITS: u64 = 4;
// const UNITS: u64 = 0;

const fn to_units(n: u64) -> u64 {
    n >> UNITS
}

const fn to_millis(n: u64) -> u64 {
    n << UNITS
}

// The time start value for each level to select the bucket at enqueue time
const fn lvl_start(lvl: u64) -> u64 {
    (LVL_SIZE - 1) << ((lvl - 1) * LVL_CLK_SHIFT)
}

// Size of each clock level
const LVL_BITS: u64 = 6;
const LVL_SIZE: u64 = 1 << LVL_BITS;
const LVL_MASK: u64 = LVL_SIZE - 1;

// Level depth
const LVL_DEPTH: u64 = 8;

const fn lvl_offs(n: u64) -> u64 {
    n * LVL_SIZE
}

// The cutoff (max. capacity of the wheel)
const WHEEL_TIMEOUT_CUTOFF: u64 = lvl_start(LVL_DEPTH);
const WHEEL_TIMEOUT_MAX: u64 = WHEEL_TIMEOUT_CUTOFF - (lvl_gran(LVL_DEPTH - 1));
const WHEEL_SIZE: usize = (LVL_SIZE as usize) * (LVL_DEPTH as usize);

// Low res time resolution
const LOWRES_RESOLUTION: Duration = Duration::from_millis(5);

const fn as_millis(dur: Duration) -> u64 {
    dur.as_secs() * 1_000 + (dur.subsec_millis() as u64)
}

/// Returns an instant corresponding to “now”.
///
/// Resolution is 5ms
#[inline]
pub fn now() -> Instant {
    TIMER.with(Timer::now)
}

/// Returns the system time corresponding to “now”.
///
/// Resolution is 5ms
#[inline]
pub fn system_time() -> SystemTime {
    TIMER.with(Timer::system_time)
}

/// Returns the system time corresponding to “now”.
///
/// If low resolution system time is not set, use system time.
/// This method does not start timer driver.
#[inline]
#[doc(hidden)]
pub fn query_system_time() -> SystemTime {
    TIMER.with(Timer::system_time)
}

#[derive(Debug)]
pub struct TimerHandle(usize);

impl TimerHandle {
    /// Createt new timer and return handle
    pub fn new(millis: u64) -> Self {
        TIMER.with(|t| t.add_timer(millis))
    }

    /// Resets the `TimerHandle` instance to a new deadline.
    pub fn reset(&self, millis: u64) {
        TIMER.with(|t| t.update_timer(self.0, millis))
    }

    pub fn is_elapsed(&self) -> bool {
        TIMER.with(|t| t.0.inner.borrow().timers[self.0].bucket.is_none())
    }

    pub fn poll_elapsed(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        TIMER.with(|t| {
            let entry = &t.0.inner.borrow().timers[self.0];
            if entry.bucket.is_none() {
                Poll::Ready(())
            } else {
                entry.task.register(cx.waker());
                Poll::Pending
            }
        })
    }
}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        TIMER.with(|t| t.remove_timer(self.0));
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub struct Flags: u8 {
        const DRIVER_STARTED = 0b0000_0001;
        const DRIVER_RECALC  = 0b0000_0010;
        const LOWRES_TIMER   = 0b0000_1000;
        const LOWRES_DRIVER  = 0b0001_0000;
        const RUNNING        = 0b0010_0000;
    }
}

thread_local! {
    static TIMER: Timer = Timer::new();
}

struct Timer(Rc<TimerInner>);

struct TimerInner {
    elapsed: Cell<u64>,
    elapsed_time: Cell<Option<Instant>>,
    next_expiry: Cell<u64>,
    flags: Cell<Flags>,
    driver: LocalWaker,
    lowres_time: Cell<Option<Instant>>,
    lowres_stime: Cell<Option<SystemTime>>,
    lowres_driver: LocalWaker,
    inner: RefCell<TimerMod>,
}

struct TimerMod {
    timers: Slab<TimerEntry>,
    driver_sleep: Delay,
    buckets: Vec<Bucket>,
    /// Bit field tracking which bucket currently contain entries.
    occupied: [u64; WHEEL_SIZE],
    lowres_driver_sleep: Delay,
}

impl Timer {
    fn new() -> Self {
        Timer(Rc::new(TimerInner {
            elapsed: Cell::new(0),
            elapsed_time: Cell::new(None),
            next_expiry: Cell::new(u64::MAX),
            flags: Cell::new(Flags::empty()),
            driver: LocalWaker::new(),
            lowres_time: Cell::new(None),
            lowres_stime: Cell::new(None),
            lowres_driver: LocalWaker::new(),
            inner: RefCell::new(TimerMod {
                buckets: Self::create_buckets(),
                timers: Slab::default(),
                driver_sleep: Delay::new(Duration::ZERO),
                occupied: [0; WHEEL_SIZE],
                lowres_driver_sleep: Delay::new(Duration::ZERO),
            }),
        }))
    }

    fn create_buckets() -> Vec<Bucket> {
        let mut buckets = Vec::with_capacity(WHEEL_SIZE);
        for idx in 0..WHEEL_SIZE {
            let lvl = idx / (LVL_SIZE as usize);
            let offs = idx % (LVL_SIZE as usize);
            buckets.push(Bucket::new(lvl, offs))
        }
        buckets
    }

    fn now(&self) -> Instant {
        if let Some(cur) = self.0.lowres_time.get() {
            cur
        } else {
            let now = Instant::now();

            let flags = self.0.flags.get();
            if flags.contains(Flags::RUNNING) {
                self.0.lowres_time.set(Some(now));

                if flags.contains(Flags::LOWRES_DRIVER) {
                    self.0.lowres_driver.wake();
                } else {
                    LowresTimerDriver::start(self.0.clone());
                }
            }
            now
        }
    }

    fn system_time(&self) -> SystemTime {
        if let Some(cur) = self.0.lowres_stime.get() {
            cur
        } else {
            let now = SystemTime::now();
            let flags = self.0.flags.get();

            if flags.contains(Flags::RUNNING) {
                self.0.lowres_stime.set(Some(now));

                if flags.contains(Flags::LOWRES_DRIVER) {
                    self.0.lowres_driver.wake();
                } else {
                    LowresTimerDriver::start(self.0.clone());
                }
            }
            now
        }
    }

    /// Add the timer into the hash bucket
    fn add_timer(&self, millis: u64) -> TimerHandle {
        if millis == 0 {
            let mut inner = self.0.inner.borrow_mut();

            let entry = inner.timers.vacant_entry();
            let no = entry.key();

            entry.insert(TimerEntry {
                bucket_entry: 0,
                bucket: None,
                task: LocalWaker::new(),
            });
            return TimerHandle(no);
        }

        let mut flags = self.0.flags.get();
        flags.insert(Flags::RUNNING);
        self.0.flags.set(flags);

        let now = self.now();
        let elapsed_time = self.0.elapsed_time();
        let delta = if now >= elapsed_time {
            to_units(as_millis(now - elapsed_time) + millis)
        } else {
            to_units(millis)
        };

        let (no, bucket_expiry) = {
            // crate timer entry
            let (idx, bucket_expiry) = self
                .0
                .calc_wheel_index(self.0.elapsed.get().wrapping_add(delta), delta);

            let no = self.0.inner.borrow_mut().add_entry(idx);
            (no, bucket_expiry)
        };

        // Check whether new bucket expire earlier
        if bucket_expiry < self.0.next_expiry.get() {
            self.0.next_expiry.set(bucket_expiry);
            if flags.contains(Flags::DRIVER_STARTED) {
                flags.insert(Flags::DRIVER_RECALC);
                self.0.flags.set(flags);
                self.0.driver.wake();
            } else {
                TimerDriver::start(self.0.clone());
            }
        }

        TimerHandle(no)
    }

    /// Update existing timer
    fn update_timer(&self, hnd: usize, millis: u64) {
        if millis == 0 {
            self.remove_timer_bucket(hnd);
            self.0.inner.borrow_mut().timers[hnd].bucket = None;
            return;
        }

        let now = self.now();
        let elapsed_time = self.0.elapsed_time();
        let delta = if now >= elapsed_time {
            max(to_units(as_millis(now - elapsed_time) + millis), 1)
        } else {
            max(to_units(millis), 1)
        };

        let bucket_expiry = {
            // calc bucket
            let (idx, bucket_expiry) = self
                .0
                .calc_wheel_index(self.0.elapsed.get().wrapping_add(delta), delta);

            self.0.inner.borrow_mut().update_entry(hnd, idx);

            bucket_expiry
        };

        // Check whether new bucket expire earlier
        if bucket_expiry < self.0.next_expiry.get() {
            self.0.next_expiry.set(bucket_expiry);
            let mut flags = self.0.flags.get();
            if flags.contains(Flags::DRIVER_STARTED) {
                flags.insert(Flags::DRIVER_RECALC);
                self.0.flags.set(flags);
                self.0.driver.wake();
            } else {
                TimerDriver::start(self.0.clone());
            }
        }
    }

    fn remove_timer(&self, handle: usize) {
        self.0.inner.borrow_mut().remove_timer_bucket(handle, true)
    }

    fn remove_timer_bucket(&self, handle: usize) {
        self.0.inner.borrow_mut().remove_timer_bucket(handle, false)
    }
}

impl TimerMod {
    fn execute_expired_timers(&mut self, mut clk: u64) {
        for lvl in 0..LVL_DEPTH {
            let idx = (clk & LVL_MASK) + lvl * LVL_SIZE;
            let b = &mut self.buckets[idx as usize];
            if !b.entries.is_empty() {
                self.occupied[b.lvl as usize] &= b.bit_n;
                for no in b.entries.drain() {
                    if let Some(timer) = self.timers.get_mut(no) {
                        timer.complete();
                    }
                }
            }

            // Is it time to look at the next level?
            if (clk & LVL_CLK_MASK) != 0 {
                break;
            }
            // Shift clock for the next level granularity
            clk >>= LVL_CLK_SHIFT;
        }
    }

    fn remove_timer_bucket(&mut self, handle: usize, remove_handle: bool) {
        let entry = &mut self.timers[handle];
        if let Some(bucket) = entry.bucket {
            let b = &mut self.buckets[bucket as usize];
            b.entries.remove(entry.bucket_entry);
            if b.entries.is_empty() {
                self.occupied[b.lvl as usize] &= b.bit_n;
            }
        }

        if remove_handle {
            self.timers.remove(handle);
        }
    }

    fn add_entry(&mut self, idx: usize) -> usize {
        let entry = self.timers.vacant_entry();
        let no = entry.key();
        let bucket = &mut self.buckets[idx];
        let bucket_entry = bucket.add_entry(no);

        entry.insert(TimerEntry {
            bucket_entry,
            bucket: Some(idx as u16),
            task: LocalWaker::new(),
        });
        self.occupied[bucket.lvl as usize] |= bucket.bit;

        no
    }

    fn update_entry(&mut self, hnd: usize, idx: usize) {
        let entry = &mut self.timers[hnd];

        // cleanup active timer
        if let Some(bucket) = entry.bucket {
            // do not do anything if wheel bucket is the same
            if idx == bucket as usize {
                return;
            }

            // remove timer entry from current bucket
            let b = &mut self.buckets[bucket as usize];
            b.entries.remove(entry.bucket_entry);
            if b.entries.is_empty() {
                self.occupied[b.lvl as usize] &= b.bit_n;
            }
        }

        // put timer to new bucket
        let bucket = &mut self.buckets[idx];
        entry.bucket = Some(idx as u16);
        entry.bucket_entry = bucket.add_entry(hnd);

        self.occupied[bucket.lvl as usize] |= bucket.bit;
    }
}

impl TimerInner {
    fn calc_wheel_index(&self, expires: u64, delta: u64) -> (usize, u64) {
        if delta < lvl_start(1) {
            Self::calc_index(expires, 0)
        } else if delta < lvl_start(2) {
            Self::calc_index(expires, 1)
        } else if delta < lvl_start(3) {
            Self::calc_index(expires, 2)
        } else if delta < lvl_start(4) {
            Self::calc_index(expires, 3)
        } else if delta < lvl_start(5) {
            Self::calc_index(expires, 4)
        } else if delta < lvl_start(6) {
            Self::calc_index(expires, 5)
        } else if delta < lvl_start(7) {
            Self::calc_index(expires, 6)
        } else if delta < lvl_start(8) {
            Self::calc_index(expires, 7)
        } else {
            // Force expire obscene large timeouts to expire at the
            // capacity limit of the wheel.
            if delta >= WHEEL_TIMEOUT_CUTOFF {
                Self::calc_index(
                    self.elapsed.get().wrapping_add(WHEEL_TIMEOUT_MAX),
                    LVL_DEPTH - 1,
                )
            } else {
                Self::calc_index(expires, LVL_DEPTH - 1)
            }
        }
    }

    /// Helper function to calculate the bucket index and bucket expiration
    fn calc_index(expires: u64, lvl: u64) -> (usize, u64) {
        // The timer wheel has to guarantee that a timer does not fire
        // early. Early expiry can happen due to:
        // - Timer is armed at the edge of a tick
        // - Truncation of the expiry time in the outer wheel levels
        //
        // Round up with level granularity to prevent this.

        let expires = (expires + lvl_gran(lvl)) >> lvl_shift(lvl);
        (
            (lvl_offs(lvl) + (expires & LVL_MASK)) as usize,
            expires << lvl_shift(lvl),
        )
    }

    fn elapsed_time(&self) -> Instant {
        if let Some(elapsed_time) = self.elapsed_time.get() {
            elapsed_time
        } else {
            let elapsed_time = Instant::now();
            self.elapsed_time.set(Some(elapsed_time));
            elapsed_time
        }
    }

    fn execute_expired_timers(&self) {
        self.inner
            .borrow_mut()
            .execute_expired_timers(self.next_expiry.get());
    }

    /// Find next expiration bucket
    fn next_pending_bucket(&self) -> Option<u64> {
        let inner = self.inner.borrow_mut();

        let mut clk = self.elapsed.get();
        let mut next = u64::MAX;

        for lvl in 0..LVL_DEPTH {
            let lvl_clk = clk & LVL_CLK_MASK;
            let occupied = inner.occupied[lvl as usize];
            let pos = if occupied == 0 {
                -1
            } else {
                let zeros = occupied
                    .rotate_right((clk & LVL_MASK) as u32)
                    .trailing_zeros() as usize;
                zeros as isize
            };

            if pos >= 0 {
                let tmp = (clk + pos as u64) << lvl_shift(lvl);
                if tmp < next {
                    next = tmp
                }

                // If the next expiration happens before we reach
                // the next level, no need to check further.
                if (pos as u64) <= ((LVL_CLK_DIV - lvl_clk) & LVL_CLK_MASK) {
                    break;
                }
            }

            clk >>= LVL_CLK_SHIFT;
            clk += u64::from(lvl_clk != 0);
        }

        if next < u64::MAX {
            Some(next)
        } else {
            None
        }
    }

    /// Get next expiry time in millis
    fn next_expiry_ms(&self) -> u64 {
        to_millis(self.next_expiry.get().saturating_sub(self.elapsed.get()))
    }

    fn stop_wheel(&self) {
        // mark all timers as elapsed
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            let mut buckets = mem::take(&mut inner.buckets);
            for b in &mut buckets {
                for no in b.entries.drain() {
                    inner.timers[no].bucket = None;
                }
            }

            // cleanup info
            self.flags.set(Flags::empty());
            self.next_expiry.set(u64::MAX);
            self.elapsed.set(0);
            self.elapsed_time.set(None);
            self.lowres_time.set(None);
            self.lowres_stime.set(None);

            inner.buckets = buckets;
            inner.occupied = [0; WHEEL_SIZE];
        }
    }
}

#[derive(Debug)]
struct Bucket {
    lvl: u32,
    bit: u64,
    bit_n: u64,
    entries: Slab<usize>,
}

impl Bucket {
    fn add_entry(&mut self, no: usize) -> usize {
        self.entries.insert(no)
    }
}

impl Bucket {
    fn new(lvl: usize, offs: usize) -> Self {
        let bit = 1 << (offs as u64);
        Bucket {
            bit,
            lvl: lvl as u32,
            bit_n: !bit,
            entries: Slab::default(),
        }
    }
}

#[derive(Debug)]
struct TimerEntry {
    bucket: Option<u16>,
    bucket_entry: usize,
    task: LocalWaker,
}

impl TimerEntry {
    fn complete(&mut self) {
        if self.bucket.is_some() {
            self.bucket.take();
            self.task.wake();
        }
    }
}

struct TimerDriver(Rc<TimerInner>);

impl TimerDriver {
    fn start(timer: Rc<TimerInner>) {
        let mut flags = timer.flags.get();
        flags.insert(Flags::DRIVER_STARTED);
        timer.flags.set(flags);
        timer.inner.borrow_mut().driver_sleep =
            Delay::new(Duration::from_millis(timer.next_expiry_ms()));

        let _ = crate::spawn(TimerDriver(timer));
    }
}

impl Drop for TimerDriver {
    fn drop(&mut self) {
        self.0.stop_wheel();
    }
}

impl Future for TimerDriver {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.0.driver.register(cx.waker());

        let mut flags = self.0.flags.get();
        if flags.contains(Flags::DRIVER_RECALC) {
            flags.remove(Flags::DRIVER_RECALC);
            self.0.flags.set(flags);

            let now = Instant::now();
            let deadline =
                if let Some(diff) = now.checked_duration_since(self.0.elapsed_time()) {
                    Duration::from_millis(self.0.next_expiry_ms()).saturating_sub(diff)
                } else {
                    Duration::from_millis(self.0.next_expiry_ms())
                };
            self.0.inner.borrow_mut().driver_sleep.reset(deadline);
        }

        loop {
            if Pin::new(&mut self.0.inner.borrow_mut().driver_sleep)
                .poll(cx)
                .is_ready()
            {
                let now = Instant::now();
                self.0.elapsed.set(self.0.next_expiry.get());
                self.0.elapsed_time.set(Some(now));
                self.0.execute_expired_timers();

                if let Some(next_expiry) = self.0.next_pending_bucket() {
                    self.0.next_expiry.set(next_expiry);
                    let dur = Duration::from_millis(self.0.next_expiry_ms());
                    self.0.inner.borrow_mut().driver_sleep.reset(dur);
                    continue;
                } else {
                    self.0.next_expiry.set(u64::MAX);
                    self.0.elapsed_time.set(None);
                }
            }
            return Poll::Pending;
        }
    }
}

struct LowresTimerDriver(Rc<TimerInner>);

impl LowresTimerDriver {
    fn start(timer: Rc<TimerInner>) {
        let mut flags = timer.flags.get();
        flags.insert(Flags::LOWRES_DRIVER);
        timer.flags.set(flags);
        timer.inner.borrow_mut().lowres_driver_sleep = Delay::new(LOWRES_RESOLUTION);

        let _ = crate::spawn(LowresTimerDriver(timer));
    }
}

impl Drop for LowresTimerDriver {
    fn drop(&mut self) {
        self.0.stop_wheel();
    }
}

impl Future for LowresTimerDriver {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.0.lowres_driver.register(cx.waker());

        let mut flags = self.0.flags.get();
        if !flags.contains(Flags::LOWRES_TIMER) {
            flags.insert(Flags::LOWRES_TIMER);
            self.0.flags.set(flags);
            self.0
                .inner
                .borrow_mut()
                .lowres_driver_sleep
                .reset(LOWRES_RESOLUTION);
        }

        if Pin::new(&mut self.0.inner.borrow_mut().lowres_driver_sleep)
            .poll(cx)
            .is_ready()
        {
            self.0.lowres_time.set(None);
            self.0.lowres_stime.set(None);
            flags.remove(Flags::LOWRES_TIMER);
            self.0.flags.set(flags);
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{interval, sleep, Millis};

    #[ntex_macros::rt_test2]
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
        let _elapsed = Instant::now() - time;
        #[cfg(not(target_os = "macos"))]
        assert!(
            _elapsed > Duration::from_millis(200) && _elapsed < Duration::from_millis(300),
            "elapsed: {:?}",
            _elapsed
        );

        fut1.await;
        let _elapsed = Instant::now() - time;
        #[cfg(not(target_os = "macos"))]
        assert!(
            _elapsed > Duration::from_millis(1000)
                && _elapsed < Duration::from_millis(1200), // osx
            "elapsed: {:?}",
            _elapsed
        );

        let time = Instant::now();
        sleep(Millis(25)).await;
        let _elapsed = Instant::now() - time;
        #[cfg(not(target_os = "macos"))]
        assert!(
            _elapsed > Duration::from_millis(20) && _elapsed < Duration::from_millis(50),
            "elapsed: {:?}",
            _elapsed
        );
    }
}
