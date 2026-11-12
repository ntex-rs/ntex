//! Utilities for tracking time.
//!
//! Use [`sleep`], [`deadline`] or [`interval`] for asynchronous waits. To run a
//! short synchronous callback without polling a timer future, retain a
//! [`CallbackTimer`].
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
use std::{cmp, future::Future, future::poll_fn, pin::Pin, task, task::Poll, time::Instant};

use ntex_rt::time::CallbackHandle;

mod types;

pub use self::types::{Millis, Seconds};
pub use ntex_rt::time::{TimerHandle, system_time};

/// Returns the start of the current runtime turn, see [`ntex_rt::now()`].
///
/// Tasks polled in the same turn observe the same instant on neon and compio,
/// on tokio this is [`Instant::now()`].
#[inline]
pub fn now() -> Instant {
    ntex_rt::now()
}

/// A reusable, local one-shot callback timer.
///
/// Each construction or reset arms one invocation of the retained callback.
/// The callback may capture `Rc` state; this handle is neither `Send` nor `Sync`
/// and cannot be cloned. Keep the handle alive until the callback should run.
/// Dropping it cancels pending delivery, including an expired callback that has
/// not started, and releases its captures. An active invocation retains its
/// captures until it finishes.
///
/// Expiration and cancellation retain the callback for reuse. The callback
/// implements `Fn`, so use interior mutability for state that changes between
/// invocations.
///
/// Callbacks run outside wheel access and may create, reset, cancel or drop
/// timers, including themselves. They must be short, synchronous and
/// non-blocking. Callbacks due on the same tick have no guaranteed order.
///
/// Callback panics propagate through the runtime. They may stop the shared
/// driver and disarm other timers; remaining callbacks need not be delivered.
/// Runtime shutdown suppresses pending delivery. A surviving handle retains
/// its callback and can be explicitly reset in a new runtime on the same thread.
/// Starting a new runtime does not automatically rearm the handle.
///
/// # Examples
///
/// Retain local state across invocations, cancel an arm, then reuse the timer:
///
/// ```
/// use std::{cell::Cell, rc::Rc};
/// use ntex::time::{CallbackTimer, Millis, sleep};
///
/// #[ntex::main]
/// async fn main() {
///     let calls = Rc::new(Cell::new(0));
///     let count = calls.clone();
///     let timer = CallbackTimer::new(Millis::ZERO, move || {
///         count.set(count.get() + 1);
///     });
///     assert_eq!(calls.get(), 0); // Zero duration never invokes inline.
///     sleep(Millis(50)).await;
///     assert!(timer.is_elapsed());
///     assert_eq!(calls.get(), 1);
///
///     timer.reset(Millis(1_000));
///     timer.cancel();
///     assert!(timer.is_elapsed());
///
///     timer.reset(Millis::ZERO);
///     sleep(Millis(50)).await;
///     assert_eq!(calls.get(), 2);
/// }
/// ```
#[derive(Debug)]
pub struct CallbackTimer(CallbackHandle);

impl CallbackTimer {
    /// Arms a callback after `duration`, using the wheel's rounding and limits.
    ///
    /// The base granularity is approximately 16 ms by default, see
    /// [`Builder::timer_resolution`](ntex_rt::Builder::timer_resolution);
    /// longer delays use coarser rounding, and delays beyond the wheel's
    /// capacity (about 24 days at 16 ms) are clamped. Callback delivery may be
    /// delayed further while the runtime is busy.
    ///
    /// A zero duration waits for a subsequent timer tick, as with [`Sleep`];
    /// the callback is never invoked inline.
    pub fn new(duration: Millis, callback: impl Fn() + 'static) -> Self {
        CallbackTimer(CallbackHandle::new(u64::from(duration.0), callback))
    }

    /// Arms another invocation, invalidating any previous pending delivery.
    ///
    /// This also suppresses an expired invocation that has been queued but has
    /// not started. It can be called before or after expiration or cancellation.
    ///
    /// A zero duration waits for a subsequent tick. Resetting from within the
    /// callback arms a new invocation, which survives completion of the old one.
    pub fn reset(&self, duration: Millis) {
        self.0.reset(u64::from(duration.0));
    }

    /// Disarms the timer and suppresses pending delivery, retaining the callback.
    ///
    /// This also suppresses an expired invocation that has not started. Calling
    /// this method on an already disarmed timer is harmless.
    ///
    /// An invocation that has already started cannot be interrupted.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Returns `true` when disarmed, including after cancellation or shutdown.
    ///
    /// Expiration marks the timer elapsed before callback delivery. This query
    /// does not indicate whether the callback has run.
    pub fn is_elapsed(&self) -> bool {
        self.0.is_elapsed()
    }
}

/// Waits until `dur` has elapsed.
///
/// No work is performed while awaiting the returned [`Sleep`]. Timers have a
/// granularity of approximately 16 milliseconds by default, see
/// [`Builder::timer_resolution`](ntex_rt::Builder::timer_resolution), and are
/// not suitable for high-resolution timing. A zero duration still waits for at
/// least one timer tick.
#[inline]
pub fn sleep<T: Into<Millis>>(dur: T) -> Sleep {
    Sleep::new(dur.into())
}

/// Waits until `dur` has elapsed.
///
/// Unlike [`sleep`], a zero-duration deadline never completes.
#[inline]
pub fn deadline<T: Into<Millis>>(dur: T) -> Deadline {
    Deadline::new(dur.into())
}

/// Creates an [`Interval`] that ticks every `period`.
///
/// The first tick completes immediately, and each later tick completes
/// `period` after the previous one was observed. An interval will tick
/// indefinitely. At any time, the [`Interval`] value can be dropped. This
/// cancels the interval.
#[inline]
pub fn interval<T: Into<Millis>>(period: T) -> Interval {
    Interval::new(period.into())
}

/// Requires a future to complete before `dur` has elapsed.
///
/// If the future completes before the duration has elapsed, then the completed
/// value is returned. Otherwise, an error is returned and the future is
/// canceled. A zero duration still represents an active timeout of at least
/// one timer tick; use [`timeout_checked`] to disable the timeout with zero. If
/// the future and timer are both ready during the same poll, the future wins.
#[inline]
pub fn timeout<T, U>(dur: U, future: T) -> Timeout<T>
where
    T: Future,
    U: Into<Millis>,
{
    Timeout::new_with_delay(future, Sleep::new(dur.into()))
}

/// Requires a future to complete before `dur` has elapsed.
///
/// If the future completes before the duration has elapsed, then the completed
/// value is returned. Otherwise, an error is returned and the future is
/// canceled. A zero duration disables the timeout.
#[inline]
pub fn timeout_checked<T, U>(dur: U, future: T) -> TimeoutChecked<T>
where
    T: Future,
    U: Into<Millis>,
{
    TimeoutChecked::new_with_delay(future, dur.into())
}

/// Future returned by [`sleep`].
///
/// # Examples
///
/// Wait 100ms and print "100 ms have elapsed".
///
/// ```
/// use ntex::time::{sleep, Millis};
///
/// #[ntex::main]
/// async fn main() {
///     sleep(Millis(100)).await;
///     println!("100 ms have elapsed");
/// }
/// ```
///
/// # Single waiter
///
/// A timer stores one waker. [`wait`](Self::wait) and
/// [`poll_elapsed`](Self::poll_elapsed) take `&self`, but only the task that
/// polled most recently is woken when the timer elapses. Wait from one task at
/// a time; an earlier concurrent waiter may stay pending after the deadline.
/// The same applies to [`Deadline`] and [`TimerHandle`].
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Sleep {
    // The link between the `Sleep` instance and the timer that drives it.
    hnd: TimerHandle,
}

impl Sleep {
    /// Creates a new sleep future.
    ///
    /// A zero duration is rounded up to 1 ms.
    #[inline]
    pub fn new(duration: Millis) -> Sleep {
        Sleep {
            hnd: TimerHandle::new(u64::from(cmp::max(duration.0, 1))),
        }
    }

    /// Returns `true` if `Sleep` has elapsed.
    #[inline]
    pub fn is_elapsed(&self) -> bool {
        self.hnd.is_elapsed()
    }

    /// Completes the timer immediately.
    #[inline]
    pub fn elapse(&self) {
        self.hnd.elapse();
    }

    /// Resets the `Sleep` instance to a new deadline.
    ///
    /// Calling this function allows changing the instant at which the `Sleep`
    /// future completes without having to create new associated state.
    ///
    /// This function can be called both before and after the future has
    /// completed. A zero duration is rounded up to 1 ms.
    pub fn reset<T: Into<Millis>>(&self, millis: T) {
        self.hnd.reset(u64::from(cmp::max(millis.into().0, 1)));
    }

    #[inline]
    /// Waits until this timer has elapsed.
    ///
    /// Only one task may wait at a time; see [single waiter](Self#single-waiter).
    pub async fn wait(&self) {
        poll_fn(|cx| self.hnd.poll_elapsed(cx)).await;
    }

    #[inline]
    /// Polls until this timer has elapsed.
    ///
    /// A pending poll replaces the previously registered waker; see
    /// [single waiter](Self#single-waiter).
    pub fn poll_elapsed(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        self.hnd.poll_elapsed(cx)
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.hnd.poll_elapsed(cx)
    }
}

/// Future returned by [`deadline`].
///
/// # Examples
///
/// Wait 100ms and print "100 ms have elapsed".
///
/// ```
/// use ntex::time::{deadline, Millis};
///
/// #[ntex::main]
/// async fn main() {
///     deadline(Millis(100)).await;
///     println!("100 ms have elapsed");
/// }
/// ```
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Deadline {
    hnd: Option<TimerHandle>,
}

impl Deadline {
    /// Creates a new deadline future.
    ///
    /// A zero duration creates a deadline that never completes.
    #[inline]
    pub fn new(duration: Millis) -> Deadline {
        if duration.0 != 0 {
            Deadline {
                hnd: Some(TimerHandle::new(u64::from(duration.0))),
            }
        } else {
            Deadline { hnd: None }
        }
    }

    #[inline]
    /// Waits until this deadline has elapsed.
    ///
    /// Only one task may wait at a time; see
    /// [single waiter](Sleep#single-waiter).
    pub async fn wait(&self) {
        poll_fn(|cx| self.poll_elapsed(cx)).await;
    }

    /// Resets the `Deadline` instance to a new deadline.
    ///
    /// Calling this function allows changing the instant at which the `Deadline`
    /// future completes without having to create new associated state.
    ///
    /// This function can be called both before and after the future has
    /// completed.
    pub fn reset<T: Into<Millis>>(&mut self, millis: T) {
        let millis = millis.into();
        if millis.0 != 0 {
            if let Some(ref mut hnd) = self.hnd {
                hnd.reset(u64::from(millis.0));
            } else {
                self.hnd = Some(TimerHandle::new(u64::from(millis.0)));
            }
        } else {
            let _ = self.hnd.take();
        }
    }

    /// Returns `true` if `Deadline` has elapsed.
    ///
    /// This agrees with polling: it returns `true` exactly when polling the
    /// deadline completes. A disabled zero-duration deadline never elapses,
    /// so it returns `false`.
    #[inline]
    pub fn is_elapsed(&self) -> bool {
        self.hnd.as_ref().is_some_and(TimerHandle::is_elapsed)
    }

    #[inline]
    /// Polls until this deadline has elapsed.
    ///
    /// A zero-duration deadline remains pending until it is reset. A pending
    /// poll replaces the previously registered waker; see
    /// [single waiter](Sleep#single-waiter).
    pub fn poll_elapsed(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        self.hnd
            .as_ref()
            .map_or(Poll::Pending, |t| t.poll_elapsed(cx))
    }
}

impl Future for Deadline {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.poll_elapsed(cx)
    }
}

pin_project_lite::pin_project! {
    /// Future returned by [`timeout`](timeout).
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    #[derive(Debug)]
    pub struct Timeout<T> {
        #[pin]
        value: T,
        delay: Sleep,
    }
}

impl<T> Timeout<T> {
    pub(crate) fn new_with_delay(value: T, delay: Sleep) -> Timeout<T> {
        Timeout { value, delay }
    }
}

impl<T> Future for Timeout<T>
where
    T: Future,
{
    type Output = Result<T::Output, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // First, try polling the future
        if let Poll::Ready(v) = this.value.poll(cx) {
            return Poll::Ready(Ok(v));
        }

        // Now check the timer
        match this.delay.poll_elapsed(cx) {
            Poll::Ready(()) => Poll::Ready(Err(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

pin_project_lite::pin_project! {
    /// Future returned by [`timeout_checked`](timeout_checked).
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct TimeoutChecked<T> {
        #[pin]
        state: TimeoutCheckedState<T>,
    }
}

pin_project_lite::pin_project! {
    #[project = TimeoutCheckedStateProject]
    enum TimeoutCheckedState<T> {
        Timeout{ #[pin] fut: Timeout<T> },
        NoTimeout{ #[pin] fut: T },
    }
}

impl<T> TimeoutChecked<T> {
    pub(crate) fn new_with_delay(value: T, delay: Millis) -> TimeoutChecked<T> {
        if delay.is_zero() {
            TimeoutChecked {
                state: TimeoutCheckedState::NoTimeout { fut: value },
            }
        } else {
            TimeoutChecked {
                state: TimeoutCheckedState::Timeout {
                    fut: Timeout::new_with_delay(value, sleep(delay)),
                },
            }
        }
    }
}

impl<T> Future for TimeoutChecked<T>
where
    T: Future,
{
    type Output = Result<T::Output, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        match self.project().state.as_mut().project() {
            TimeoutCheckedStateProject::Timeout { fut } => fut.poll(cx),
            TimeoutCheckedStateProject::NoTimeout { fut } => fut.poll(cx).map(Result::Ok),
        }
    }
}

/// An interval returned by [`interval`].
///
/// This type allows you to wait on a sequence of instants with a certain
/// duration between each instant.
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Debug)]
pub struct Interval {
    hnd: TimerHandle,
    period: u32,
}

impl Interval {
    /// Creates an interval with the specified period.
    ///
    /// The first tick completes immediately. A zero period is rounded up to
    /// 1 ms.
    #[inline]
    pub fn new(period: Millis) -> Interval {
        Interval {
            hnd: TimerHandle::new(0),
            period: cmp::max(period.0, 1),
        }
    }

    #[inline]
    /// Waits for the next interval tick.
    pub async fn tick(&self) {
        poll_fn(|cx| self.poll_tick(cx)).await;
    }

    #[inline]
    /// Polls for the next interval tick.
    pub fn poll_tick(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        if self.hnd.poll_elapsed(cx).is_ready() {
            self.hnd.reset(u64::from(self.period));
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl crate::Stream for Interval {
    type Item = ();

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_tick(cx).map(|()| Some(()))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use std::{future::poll_fn, rc::Rc, time};

    use super::*;
    use crate::future::lazy;

    /// STATEMENT: Cached `now()` is not in the future and stays close to the current instant.
    ///
    /// HOW:
    /// - Sleep, then compare `now()` with `Instant::now`.
    /// - Verify the cached value is no later and less than 50 milliseconds behind.
    #[ntex::test]
    async fn now_is_current() {
        sleep(Millis(25)).await;

        let now = now();
        let cur = time::Instant::now();
        assert!(cur >= now);
        assert!(cur - now < time::Duration::from_millis(50));
    }

    /// STATEMENT: Cached `now()` advances across an awaited sleep.
    ///
    /// HOW:
    /// - Read `now()` before and after a 25-millisecond sleep.
    /// - Verify the second value advances by at least that duration.
    #[ntex::test]
    async fn now_updates_after_sleep() {
        sleep(Millis(50)).await;

        let first_time = now();

        sleep(Millis(25)).await;

        let second_time = now();
        assert!(second_time - first_time >= time::Duration::from_millis(25));
    }

    /// STATEMENT: Back-to-back system-time reads share the cached value.
    ///
    /// HOW:
    /// - Let the runtime advance, then compare two consecutive `system_time()` calls.
    #[ntex::test]
    async fn system_time_service_time_does_not_immediately_change() {
        sleep(Seconds(1)).await;

        assert_eq!(system_time(), system_time());
    }

    /// STATEMENT: Cached system time advances after sleeping beyond its refresh interval.
    ///
    /// HOW:
    /// - Read system time around a 600-millisecond sleep.
    /// - Compare epoch-relative durations and require at least that much advancement.
    #[ntex::test]
    async fn system_time_service_time_updates_after_resolution_interval() {
        sleep(Millis(100)).await;

        let wait_time = 600;

        let first_time = system_time()
            .duration_since(time::SystemTime::UNIX_EPOCH)
            .unwrap();

        sleep(Millis(wait_time)).await;

        let second_time = system_time()
            .duration_since(time::SystemTime::UNIX_EPOCH)
            .unwrap();

        assert!(
            second_time.checked_sub(first_time).unwrap()
                >= time::Duration::from_millis(u64::from(wait_time))
        );
    }

    /// STATEMENT: Zero sleeps round up, while explicitly elapsed timers complete promptly.
    ///
    /// HOW:
    /// - Measure zero, one-millisecond, and reset-to-zero sleeps and check their elapsed state.
    /// - Await an already-elapsed handle and a manually elapsed shared sleep within short bounds.
    #[ntex::test]
    async fn test_sleep_0() {
        sleep(Seconds(1)).await;

        let first_time = time::Instant::now();
        sleep(Millis(0)).await;
        let second_time = time::Instant::now();
        assert!(second_time - first_time >= time::Duration::from_millis(1));

        let first_time = time::Instant::now();
        sleep(Millis(1)).await;
        let second_time = time::Instant::now();
        assert!(second_time - first_time >= time::Duration::from_millis(1));

        let first_time = time::Instant::now();
        let fut = sleep(Millis(10000));
        assert!(!fut.is_elapsed());
        // a zero delay is rounded up to 1 ms
        fut.reset(Millis::ZERO);
        assert!(!fut.is_elapsed());
        fut.await;
        let second_time = time::Instant::now();
        assert!(second_time - first_time >= time::Duration::from_millis(1));

        let fut = sleep(Millis::ZERO);
        assert!(!fut.is_elapsed());
        fut.await;

        // `now()` may advance between two calls, measure
        // with the clock. An elapsed timer completes without waiting for the
        // timer driver, the bound leaves room for scheduling latency
        let first_time = time::Instant::now();
        let fut = Sleep {
            hnd: TimerHandle::new(0),
        };
        assert!(fut.is_elapsed());
        fut.await;
        let second_time = time::Instant::now();
        assert!(second_time - first_time < time::Duration::from_millis(50));

        let first_time = time::Instant::now();
        let fut = Rc::new(sleep(Millis(10_0000)));
        let s = fut.clone();
        ntex::rt::spawn(async move {
            s.elapse();
        });
        poll_fn(|cx| fut.poll_elapsed(cx)).await;
        assert!(fut.is_elapsed());
        let second_time = time::Instant::now();
        assert!(second_time - first_time < time::Duration::from_millis(50));
    }

    /// STATEMENT: `Deadline::is_elapsed()` agrees with polling for disabled, active, reset and
    /// expired deadlines.
    ///
    /// HOW:
    /// - Create a zero deadline and assert `is_elapsed()` is `false`, polling stays pending, and
    ///   `timeout()` around it times out.
    /// - Create a 50 ms deadline and assert neither the predicate nor polling reports elapsed.
    /// - Await a 1 ms deadline and assert the predicate is `true` and polling is ready.
    /// - Reset the expired deadline to 50 ms and assert both report not elapsed again, then reset
    ///   it to zero and assert both still report not elapsed after 20 ms.
    #[ntex::test]
    async fn test_deadline_is_elapsed_matches_polling() {
        let dl = deadline(Millis::ZERO);
        assert!(!dl.is_elapsed());
        assert!(lazy(|cx| dl.poll_elapsed(cx)).await.is_pending());
        assert!(timeout(Millis(20), dl.wait()).await.is_err());
        assert!(!dl.is_elapsed());

        let dl = deadline(Millis(50));
        assert!(!dl.is_elapsed());
        assert!(lazy(|cx| dl.poll_elapsed(cx)).await.is_pending());

        let mut dl = deadline(Millis(1));
        dl.wait().await;
        assert!(dl.is_elapsed());
        assert!(lazy(|cx| dl.poll_elapsed(cx)).await.is_ready());

        dl.reset(Millis(50));
        assert!(!dl.is_elapsed());
        assert!(lazy(|cx| dl.poll_elapsed(cx)).await.is_pending());

        dl.reset(Millis::ZERO);
        sleep(Millis(20)).await;
        assert!(!dl.is_elapsed());
        assert!(lazy(|cx| dl.poll_elapsed(cx)).await.is_pending());
    }

    /// STATEMENT: Deadlines can be extended, disabled with zero, and re-enabled by reset.
    ///
    /// HOW:
    /// - Measure initial and extended deadlines and time out a zero deadline.
    /// - Poll disabled and re-enabled deadlines, then inspect debug output.
    #[ntex::test]
    async fn test_deadline() {
        sleep(Seconds(1)).await;

        let first_time = now();
        let dl = deadline(Millis(1));
        dl.await;
        let second_time = now();
        assert!(second_time - first_time >= time::Duration::from_millis(1));
        assert!(timeout(Millis(100), deadline(Millis(0))).await.is_err());

        let mut dl = deadline(Millis(1));
        dl.reset(Millis::ZERO);
        assert!(lazy(|cx| dl.poll_elapsed(cx)).await.is_pending());

        let mut dl = deadline(Millis(1));
        dl.reset(Millis(100));
        let first_time = now();
        dl.await;
        let second_time = now();
        assert!(second_time - first_time >= time::Duration::from_millis(100));

        let mut dl = deadline(Millis(0));
        dl.reset(Millis(1));
        assert!(lazy(|cx| dl.poll_elapsed(cx)).await.is_pending());

        assert!(format!("{dl:?}").contains("Deadline"));
    }

    /// STATEMENT: A zero-period interval waits between ticks instead of staying immediately ready.
    ///
    /// HOW:
    /// - Consume the first tick, verify the next poll is pending, and await another tick.
    #[ntex::test]
    async fn test_interval_zero_period() {
        let int = interval(Millis::ZERO);
        int.tick().await;

        // a zero period is rounded up to 1 ms instead of ticking on every poll
        assert!(lazy(|cx| int.poll_tick(cx)).await.is_pending());
        int.tick().await;
    }

    /// STATEMENT: Intervals tick immediately once, then wait for their period through both APIs.
    ///
    /// HOW:
    /// - Measure the first and second explicit ticks of a 250-millisecond interval.
    /// - Receive another tick through the stream API and check the same timing bounds.
    #[ntex::test]
    async fn test_interval() {
        let mut int = interval(Millis(250));

        // the first tick completes immediately
        let time = time::Instant::now();
        int.tick().await;
        assert!(time.elapsed() < time::Duration::from_millis(50));

        let time = time::Instant::now();
        int.tick().await;
        let elapsed = time.elapsed();
        assert!(
            elapsed > time::Duration::from_millis(200)
                && elapsed < time::Duration::from_millis(450),
            "elapsed: {elapsed:?}"
        );

        let time = time::Instant::now();
        int.next().await;
        let elapsed = time.elapsed();
        assert!(
            elapsed > time::Duration::from_millis(200)
                && elapsed < time::Duration::from_millis(450),
            "elapsed: {elapsed:?}"
        );
    }

    /// STATEMENT: A one-second interval keeps subsequent ticks within the expected timing bounds.
    ///
    /// HOW:
    /// - Consume the immediate tick, then measure three ticks between one and 1.5 seconds each.
    #[ntex::test]
    async fn test_interval_one_sec() {
        let int = interval(Millis::ONE_SEC);
        int.tick().await;

        for _i in 0..3 {
            let time = time::Instant::now();
            int.tick().await;
            let elapsed = time.elapsed();
            assert!(
                elapsed > time::Duration::from_secs(1)
                    && elapsed < time::Duration::from_millis(1500),
                "elapsed: {elapsed:?}"
            );
        }
    }

    /// STATEMENT: Checked timeouts preserve timely results, expire pending work, and disable at zero.
    ///
    /// HOW:
    /// - Run a shorter sleep under a deadline and a permanently pending future under a short one.
    /// - Verify a zero timeout allows a delayed future to finish.
    #[ntex::test]
    async fn test_timeout_checked() {
        let result = timeout_checked(Millis(200), sleep(Millis(100))).await;
        assert!(result.is_ok());

        // a future that never completes, a late timer driver would elapse
        // a competing sleep in the same pass
        let result = timeout_checked(Millis(5), std::future::pending::<()>()).await;
        assert!(result.is_err());

        let result = timeout_checked(Millis(0), sleep(Millis(100))).await;
        assert!(result.is_ok());
    }

    /// STATEMENT: Sleep and deadline wait helpers complete except for a disabled zero deadline.
    ///
    /// HOW:
    /// - Await both helpers and check the clock has not moved backward.
    /// - Wrap a zero deadline's wait in a timeout and verify it never completes first.
    #[ntex::test]
    async fn sleep_and_deadline_wait() {
        let start = now();
        sleep(Millis(5)).wait().await;
        deadline(Millis(5)).wait().await;
        assert!(now() >= start);

        let hnd = Deadline::new(Millis::ZERO);
        assert!(
            crate::time::timeout(Millis(20), hnd.wait()).await.is_err(),
            "zero deadline never completes"
        );
    }

    /// STATEMENT: `Sleep` and `Deadline` wake only the most recently registered task
    /// when they elapse, and a superseded waiter still observes completion.
    ///
    /// HOW:
    /// - Poll each long timer with two distinct counted wakers, then `elapse` it.
    /// - Check only the second waker was woken once and re-polling with the first
    ///   is ready.
    #[ntex::test]
    async fn single_waiter_wakes_latest_task_only() {
        use crate::task::hooks::counted_waker;

        let poll_pair = |poll: &dyn Fn(&mut task::Context<'_>) -> Poll<()>, elapse: &dyn Fn()| {
            let (first, first_waker) = counted_waker();
            let (second, second_waker) = counted_waker();
            let mut first_cx = task::Context::from_waker(&first_waker);
            let mut second_cx = task::Context::from_waker(&second_waker);

            assert_eq!(poll(&mut first_cx), Poll::Pending);
            assert_eq!(poll(&mut second_cx), Poll::Pending);
            elapse();
            assert_eq!((first.get(), second.get()), (0, 1));
            assert_eq!(poll(&mut first_cx), Poll::Ready(()));
        };

        let slp = Sleep::new(Millis(10_000));
        poll_pair(&|cx| slp.poll_elapsed(cx), &|| slp.elapse());

        let dl = Deadline::new(Millis(10_000));
        let hnd = dl.hnd.as_ref().unwrap();
        poll_pair(&|cx| dl.poll_elapsed(cx), &|| hnd.elapse());
    }

    /// STATEMENT: Timers with different deadlines progress alongside a recurring interval.
    ///
    /// HOW:
    /// - Spawn an interval task and await shorter and longer sleeps followed by another short sleep.
    /// - On non-macOS targets, verify each elapsed duration stays within its configured bounds.
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
                _elapsed > time::Duration::from_millis(200)
                    && _elapsed < time::Duration::from_millis(300),
                "elapsed: {_elapsed:?}"
            );
        }

        fut1.await;

        #[cfg(not(target_os = "macos"))]
        {
            let _elapsed = time.elapsed();
            assert!(
                _elapsed > time::Duration::from_secs(1)
                    && _elapsed < time::Duration::from_millis(1200), // osx
                "elapsed: {_elapsed:?}",
            );
        }

        let time = Instant::now();
        sleep(Millis(25)).await;
        #[cfg(not(target_os = "macos"))]
        {
            let _elapsed = time.elapsed();
            assert!(
                _elapsed > time::Duration::from_millis(20)
                    && _elapsed < time::Duration::from_millis(50),
                "elapsed: {_elapsed:?}",
            );
        }
    }

    /// STATEMENT: The timer futures and `CallbackTimer` hold a thread-local timer
    /// handle, so they are not `Send`.
    ///
    /// HOW:
    /// - Define a trait with a blanket impl for all types and a second impl for
    ///   `Send` types.
    /// - Resolve the trait item with an inferred parameter for `Sleep`, `Deadline`,
    ///   `Interval`, and `CallbackTimer`; it compiles only if exactly one impl
    ///   applies, i.e. the type is not `Send`.
    #[test]
    fn timer_futures_are_thread_local() {
        trait NotSend<A> {
            fn check() {}
        }
        impl<T: ?Sized> NotSend<()> for T {}
        impl<T: ?Sized + Send> NotSend<u8> for T {}

        <Sleep as NotSend<_>>::check();
        <Deadline as NotSend<_>>::check();
        <Interval as NotSend<_>>::check();
        <CallbackTimer as NotSend<_>>::check();
    }
}
