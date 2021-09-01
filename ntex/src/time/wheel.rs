//! Time wheel based timer service.
//!
//! Inspired by linux kernel timers system
#![allow(arithmetic_overflow)]
use std::cell::{Cell, RefCell};
use std::{future::Future, mem, pin::Pin, rc::Rc, task, task::Poll, time};

use slab::Slab;

use crate::rt::time_driver::{sleep_until, Sleep};
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
const ONE_MS: time::Duration = time::Duration::from_millis(1);

/// Returns an instant corresponding to “now”.
///
/// Resolution is ~1ms
#[inline]
pub fn now() -> time::Instant {
    TIMER.with(|t| t.0.borrow().now())
}

/// Returns the system time corresponding to “now”.
///
/// Resolution is ~1ms
#[inline]
pub fn system_time() -> time::SystemTime {
    TIMER.with(|t| t.0.borrow().system_time())
}

#[derive(Debug)]
pub struct TimerHandle(usize);

impl TimerHandle {
    pub fn new(millis: u64) -> Self {
        Timer::add_timer(millis)
    }

    /// Resets the `TimerHandle` instance to a new deadline.
    pub fn reset(&self, millis: u64) {
        Timer::update_timer(self.0, millis);
    }

    pub fn is_elapsed(&self) -> bool {
        Timer::with_entry(self.0, |entry| {
            entry.flags.contains(TimerEntryFlags::ELAPSED)
        })
    }

    pub fn poll_elapsed(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        Timer::with_entry(self.0, |entry| {
            if entry.flags.contains(TimerEntryFlags::ELAPSED) {
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
        Timer::remove_timer(self.0);
    }
}

bitflags::bitflags! {
    pub struct Flags: u8 {
        const DRIVER_STARTED = 0b0000_0001;
        const NEEDS_RECALC   = 0b0000_0010;
        const TIMER_ACTIVE   = 0b0000_0100;
        const LOWRES_TIMER   = 0b0000_1000;
    }
}

struct Timer(Rc<RefCell<TimerInner>>);

thread_local! {
    static TIMER: Timer = Timer::new();
}

struct TimerInner {
    timers: Slab<TimerEntry>,
    elapsed: u64,
    elapsed_instant: time::Instant,
    next_expiry: u64,
    flags: Flags,
    driver: LocalWaker,
    buckets: Vec<Bucket>,
    /// Bit field tracking which bucket currently contain entries.
    occupied: [u64; WHEEL_SIZE],
    lowres_time: Cell<Option<time::Instant>>,
    lowres_stime: Cell<Option<time::SystemTime>>,
    lowres_driver: LocalWaker,
}

impl Timer {
    fn new() -> Self {
        let inner = Rc::new(RefCell::new(TimerInner::new()));
        LowresTimerDriver::start(&inner);
        Timer(inner)
    }

    fn with_entry<F, R>(no: usize, f: F) -> R
    where
        F: Fn(&mut TimerEntry) -> R,
    {
        TIMER.with(|t| f(&mut t.0.borrow_mut().timers[no]))
    }

    // Add the timer into the hash bucket
    fn add_timer(expires: u64) -> TimerHandle {
        TIMER.with(|t| TimerInner::add_timer(&t.0, expires))
    }

    fn update_timer(handle: usize, expires: u64) {
        TIMER.with(|t| TimerInner::update_timer(&t.0, handle, expires));
    }

    fn remove_timer(handle: usize) {
        TIMER.with(|t| t.0.borrow_mut().remove_timer(handle));
    }
}

impl TimerInner {
    fn new() -> Self {
        TimerInner {
            buckets: Self::create_buckets(),
            timers: Slab::default(),
            elapsed: 0,
            elapsed_instant: time::Instant::now(),
            next_expiry: u64::MAX,
            flags: Flags::empty(),
            driver: LocalWaker::new(),
            occupied: [0; WHEEL_SIZE],
            lowres_time: Cell::new(None),
            lowres_stime: Cell::new(None),
            lowres_driver: LocalWaker::new(),
        }
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

    fn now(&self) -> time::Instant {
        let cur = self.lowres_time.get();
        if let Some(cur) = cur {
            cur
        } else {
            let now = time::Instant::now();
            self.lowres_driver.wake();
            self.lowres_time.set(Some(now));
            now
        }
    }

    fn system_time(&self) -> time::SystemTime {
        let cur = self.lowres_stime.get();
        if let Some(cur) = cur {
            cur
        } else {
            let now = time::SystemTime::now();
            self.lowres_driver.wake();
            self.lowres_stime.set(Some(now));
            now
        }
    }

    // Add the timer into the hash bucket
    fn add_timer(inner: &Rc<RefCell<Self>>, millis: u64) -> TimerHandle {
        let mut slf = inner.borrow_mut();
        if millis == 0 {
            let entry = slf.timers.vacant_entry();
            let no = entry.key();

            entry.insert(TimerEntry {
                bucket_entry: 0,
                bucket: 0,
                task: LocalWaker::new(),
                flags: TimerEntryFlags::ELAPSED,
            });
            return TimerHandle(no);
        }

        let delta = to_units(
            (slf.now() + time::Duration::from_millis(millis) - slf.elapsed_instant)
                .as_millis() as u64,
        );

        let (no, bucket_expiry) = {
            let slf = &mut *slf;

            // crate timer entry
            let (idx, bucket_expiry) = slf.calc_wheel_index(slf.elapsed + delta, delta);
            let entry = slf.timers.vacant_entry();
            let no = entry.key();
            let bucket = &mut slf.buckets[idx];
            let bucket_entry = bucket.add_entry(no);

            entry.insert(TimerEntry {
                bucket_entry,
                bucket: idx as u16,
                task: LocalWaker::new(),
                flags: TimerEntryFlags::empty(),
            });
            slf.occupied[bucket.lvl as usize] |= bucket.bit;
            (no, bucket_expiry)
        };

        // Check whether new bucket expire earlier
        if bucket_expiry < slf.next_expiry {
            slf.next_expiry = bucket_expiry;
            if !slf.flags.contains(Flags::DRIVER_STARTED) {
                slf.flags.insert(Flags::DRIVER_STARTED);
                drop(slf);
                TimerDriver::start(inner);
            } else {
                slf.flags.insert(Flags::NEEDS_RECALC);
                slf.driver.wake();
            }
        }

        TimerHandle(no)
    }

    fn update_timer(inner: &Rc<RefCell<Self>>, hnd: usize, millis: u64) {
        let mut slf = inner.borrow_mut();
        if millis == 0 {
            slf.timers[hnd].flags = TimerEntryFlags::ELAPSED;
            return;
        }

        let delta = to_units(
            (slf.now() + time::Duration::from_millis(millis) - slf.elapsed_instant)
                .as_millis() as u64,
        );

        let bucket_expiry = {
            let slf = &mut *slf;

            // calc buckeet
            let (idx, bucket_expiry) = slf.calc_wheel_index(slf.elapsed + delta, delta);

            let entry = &mut slf.timers[hnd];

            // do not do anything if  bucket is the same
            if idx == entry.bucket as usize {
                return;
            }

            if !entry.flags.contains(TimerEntryFlags::ELAPSED) {
                let b = &mut slf.buckets[entry.bucket as usize];
                b.entries.remove(entry.bucket_entry);
                if b.entries.is_empty() {
                    slf.occupied[b.lvl as usize] &= b.bit_n;
                }
            }

            let bucket = &mut slf.buckets[idx];
            let bucket_entry = bucket.add_entry(hnd);

            entry.bucket = idx as u16;
            entry.bucket_entry = bucket_entry;
            entry.flags = TimerEntryFlags::empty();

            slf.occupied[bucket.lvl as usize] |= bucket.bit;
            bucket_expiry
        };

        // Check whether new bucket expire earlier
        if bucket_expiry < slf.next_expiry {
            slf.next_expiry = bucket_expiry;
            if !slf.flags.contains(Flags::DRIVER_STARTED) {
                slf.flags.insert(Flags::DRIVER_STARTED);
                drop(slf);
                TimerDriver::start(inner);
            } else {
                slf.flags.insert(Flags::NEEDS_RECALC);
                slf.driver.wake();
            }
        }
    }

    fn remove_timer(&mut self, handle: usize) {
        let entry = self.timers.remove(handle);

        if !entry.flags.contains(TimerEntryFlags::ELAPSED) {
            let b = &mut self.buckets[entry.bucket as usize];
            b.entries.remove(entry.bucket_entry);
            if b.entries.is_empty() {
                self.occupied[b.lvl as usize] &= b.bit_n;
            }
        }
    }

    // Find next expiration bucket
    fn next_pending_bucket(&mut self) -> Option<u64> {
        let mut clk = self.elapsed;
        let mut next = u64::MAX;

        for lvl in 0..LVL_DEPTH {
            let lvl_clk = clk & LVL_CLK_MASK;
            let occupied = self.occupied[lvl as usize];
            let pos = if occupied == 0 {
                -1
            } else {
                let zeros = occupied
                    .rotate_right((clk & LVL_MASK) as u32)
                    .trailing_zeros() as usize;
                zeros as isize
            };

            if pos >= 0 {
                let tmp = (clk + pos as u64) << lvl_shift(lvl as u64);
                if tmp < next {
                    next = tmp
                }

                // If the next expiration happens before we reach
                // the next level, no need to check further.
                if (pos as u64) <= ((LVL_CLK_DIV - lvl_clk) & LVL_CLK_MASK) {
                    break;
                }
            }

            let adj = if lvl_clk == 0 { 0 } else { 1 };
            clk >>= LVL_CLK_SHIFT;
            clk += adj;
        }

        if next < u64::MAX {
            Some(next)
        } else {
            None
        }
    }

    // Get instant of the next expiry
    fn next_expiry(&self) -> time::Instant {
        let millis = to_millis(self.next_expiry - self.elapsed);
        self.now() + time::Duration::from_millis(millis)
    }

    fn execute_expired_timers(&mut self, instant: time::Instant) {
        let mut clk = self.next_expiry;
        self.elapsed = self.next_expiry;
        self.elapsed_instant = instant;

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
                Self::calc_index(self.elapsed + WHEEL_TIMEOUT_MAX, LVL_DEPTH - 1)
            } else {
                Self::calc_index(expires, LVL_DEPTH - 1)
            }
        }
    }

    // Helper function to calculate the bucket index and bucket expiration
    fn calc_index(expires2: u64, lvl: u64) -> (usize, u64) {
        /*
         * The timer wheel has to guarantee that a timer does not fire
         * early. Early expiry can happen due to:
         * - Timer is armed at the edge of a tick
         * - Truncation of the expiry time in the outer wheel levels
         *
         * Round up with level granularity to prevent this.
         */

        let expires = (expires2 + lvl_gran(lvl)) >> lvl_shift(lvl);
        (
            (lvl_offs(lvl) + (expires & LVL_MASK)) as usize,
            expires << lvl_shift(lvl),
        )
    }

    fn stop_wheel(&mut self) {
        // mark all old timers as elapsed
        let mut buckets = mem::take(&mut self.buckets);
        for b in &mut buckets {
            for no in b.entries.drain() {
                self.timers[no].flags.insert(TimerEntryFlags::ELAPSED);
            }
        }

        // cleanup info
        self.flags = Flags::empty();
        self.buckets = buckets;
        self.occupied = [0; WHEEL_SIZE];
        self.next_expiry = u64::MAX;
        self.elapsed = 0;
        self.elapsed_instant = time::Instant::now();
        self.lowres_time.set(None);
        self.lowres_stime.set(None);
    }
}

#[derive(Debug)]
struct Bucket {
    lvl: u32,
    offs: u32,
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
            offs: offs as u32,
            bit_n: !bit,
            entries: Slab::default(),
        }
    }
}

bitflags::bitflags! {
    pub struct TimerEntryFlags: u8 {
        const ELAPSED = 0b0000_0001;
    }
}

#[derive(Debug)]
struct TimerEntry {
    flags: TimerEntryFlags,
    bucket: u16,
    bucket_entry: usize,
    task: LocalWaker,
}

impl TimerEntry {
    fn complete(&mut self) {
        if !self.flags.contains(TimerEntryFlags::ELAPSED) {
            self.flags.insert(TimerEntryFlags::ELAPSED);
            self.task.wake();
        }
    }
}

struct TimerDriver {
    inner: Rc<RefCell<TimerInner>>,
    sleep: Pin<Box<Sleep>>,
}

impl TimerDriver {
    fn start(cell: &Rc<RefCell<TimerInner>>) {
        let mut inner = cell.borrow_mut();
        inner.flags.insert(Flags::TIMER_ACTIVE);

        crate::rt::spawn(TimerDriver {
            inner: cell.clone(),
            sleep: Box::pin(sleep_until(inner.next_expiry())),
        });
    }
}

impl Drop for TimerDriver {
    fn drop(&mut self) {
        self.inner.borrow_mut().stop_wheel();
    }
}

impl Future for TimerDriver {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.inner.borrow_mut();
        inner.driver.register(cx.waker());

        if inner.flags.contains(Flags::NEEDS_RECALC) {
            inner.flags.remove(Flags::NEEDS_RECALC);
            inner.flags.insert(Flags::TIMER_ACTIVE);
            let exp = inner.next_expiry();
            drop(inner);
            Pin::as_mut(&mut self.sleep).reset(exp);
            return self.poll(cx);
        } else if inner.flags.contains(Flags::TIMER_ACTIVE) {
            drop(inner);
            let result = Pin::as_mut(&mut self.sleep).poll(cx).is_ready();
            if result {
                let instant = self.sleep.deadline();
                let mut inner = self.inner.borrow_mut();
                inner.execute_expired_timers(instant);

                if let Some(next_expiry) = inner.next_pending_bucket() {
                    inner.next_expiry = next_expiry;
                    inner.flags.insert(Flags::TIMER_ACTIVE);
                    let exp = inner.next_expiry();
                    drop(inner);
                    Pin::as_mut(&mut self.sleep).reset(exp);
                    return self.poll(cx);
                } else {
                    inner.next_expiry = u64::MAX;
                    inner.flags.remove(Flags::TIMER_ACTIVE);
                }
            }
        }
        Poll::Pending
    }
}

struct LowresTimerDriver {
    inner: Rc<RefCell<TimerInner>>,
    sleep: Pin<Box<Sleep>>,
}

impl LowresTimerDriver {
    fn start(cell: &Rc<RefCell<TimerInner>>) {
        let mut inner = cell.borrow_mut();
        inner.flags.insert(Flags::LOWRES_TIMER);

        crate::rt::spawn(LowresTimerDriver {
            inner: cell.clone(),
            sleep: Box::pin(sleep_until(time::Instant::now() + ONE_MS)),
        });
    }
}

impl Drop for LowresTimerDriver {
    fn drop(&mut self) {
        self.inner.borrow_mut().stop_wheel();
    }
}

impl Future for LowresTimerDriver {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.inner.borrow_mut();
        inner.lowres_driver.register(cx.waker());

        if inner.flags.contains(Flags::LOWRES_TIMER) {
            drop(inner);
            if Pin::as_mut(&mut self.sleep).poll(cx).is_ready() {
                let mut inner = self.inner.borrow_mut();
                inner.lowres_time.set(None);
                inner.lowres_stime.set(None);
                inner.flags.remove(Flags::LOWRES_TIMER);
            }
            task::Poll::Pending
        } else {
            inner.flags.insert(Flags::LOWRES_TIMER);
            drop(inner);
            Pin::as_mut(&mut self.sleep).reset(time::Instant::now() + ONE_MS);
            self.poll(cx)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::time::*;

    #[crate::rt_test]
    async fn test_timer() {
        crate::rt::spawn(async {
            let s = interval(Millis(25));
            loop {
                s.tick().await;
            }
        });
        let time = Instant::now();
        let fut1 = sleep(Millis(1000));
        let fut2 = sleep(Millis(200));

        fut2.await;
        let elapsed = Instant::now() - time;
        assert!(
            elapsed > Duration::from_millis(200) && elapsed < Duration::from_millis(250)
        );

        fut1.await;
        let elapsed = Instant::now() - time;
        assert!(
            elapsed > Duration::from_millis(1000)
                && elapsed < Duration::from_millis(1200)
        );

        let time = Instant::now();
        sleep(Millis(25)).await;
        let elapsed = Instant::now() - time;
        assert!(
            elapsed > Duration::from_millis(20) && elapsed < Duration::from_millis(45)
        );
    }
}
