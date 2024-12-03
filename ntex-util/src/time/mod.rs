//! Utilities for tracking time.
use std::{cmp, future::poll_fn, future::Future, pin::Pin, task, task::Poll};

mod types;
mod wheel;

pub use self::types::{Millis, Seconds};
pub use self::wheel::{now, query_system_time, system_time, TimerHandle};

/// Waits until `duration` has elapsed.
///
/// No work is performed while awaiting on the sleep future to complete. `Sleep`
/// operates at 16 millisecond granularity and should not be used for tasks that
/// require high-resolution timers. `Sleep` sleeps at least one tick (16 millis)
/// even if 0 millis duration is used.
#[inline]
pub fn sleep<T: Into<Millis>>(dur: T) -> Sleep {
    Sleep::new(dur.into())
}

/// Waits until `duration` has elapsed.
///
/// This is similar to `sleep` future, but in case of `0` duration deadline future
/// never completes.
#[inline]
pub fn deadline<T: Into<Millis>>(dur: T) -> Deadline {
    Deadline::new(dur.into())
}

/// Creates new [`Interval`] that yields with interval of `period`.
///
/// An interval will tick indefinitely. At any time, the [`Interval`] value can
/// be dropped. This cancels the interval.
#[inline]
pub fn interval<T: Into<Millis>>(period: T) -> Interval {
    Interval::new(period.into())
}

/// Require a `Future` to complete before the specified duration has elapsed.
///
/// If the future completes before the duration has elapsed, then the completed
/// value is returned. Otherwise, an error is returned and the future is
/// canceled.
#[inline]
pub fn timeout<T, U>(dur: U, future: T) -> Timeout<T>
where
    T: Future,
    U: Into<Millis>,
{
    Timeout::new_with_delay(future, Sleep::new(dur.into()))
}

/// Require a `Future` to complete before the specified duration has elapsed.
///
/// If the future completes before the duration has elapsed, then the completed
/// value is returned. Otherwise, an error is returned and the future is
/// canceled. If duration value is zero then timeout is disabled.
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
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Sleep {
    // The link between the `Sleep` instance and the timer that drives it.
    hnd: TimerHandle,
}

impl Sleep {
    /// Create new sleep future
    #[inline]
    pub fn new(duration: Millis) -> Sleep {
        Sleep {
            hnd: TimerHandle::new(cmp::max(duration.0, 1) as u64),
        }
    }

    /// Returns `true` if `Sleep` has elapsed.
    #[inline]
    pub fn is_elapsed(&self) -> bool {
        self.hnd.is_elapsed()
    }

    /// Complete sleep timer.
    #[inline]
    pub fn elapse(&self) {
        self.hnd.elapse()
    }

    /// Resets the `Sleep` instance to a new deadline.
    ///
    /// Calling this function allows changing the instant at which the `Sleep`
    /// future completes without having to create new associated state.
    ///
    /// This function can be called both before and after the future has
    /// completed.
    pub fn reset<T: Into<Millis>>(&self, millis: T) {
        self.hnd.reset(millis.into().0 as u64);
    }

    #[inline]
    /// Wait when `Sleep` instance get elapsed.
    pub async fn wait(&self) {
        poll_fn(|cx| self.hnd.poll_elapsed(cx)).await
    }

    #[inline]
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
    /// Create new deadline future
    #[inline]
    pub fn new(duration: Millis) -> Deadline {
        if duration.0 != 0 {
            Deadline {
                hnd: Some(TimerHandle::new(duration.0 as u64)),
            }
        } else {
            Deadline { hnd: None }
        }
    }

    #[inline]
    /// Wait when `Sleep` instance get elapsed.
    pub async fn wait(&self) {
        poll_fn(|cx| self.poll_elapsed(cx)).await
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
                hnd.reset(millis.0 as u64);
            } else {
                self.hnd = Some(TimerHandle::new(millis.0 as u64));
            }
        } else {
            let _ = self.hnd.take();
        }
    }

    /// Returns `true` if `Deadline` has elapsed.
    #[inline]
    pub fn is_elapsed(&self) -> bool {
        self.hnd.as_ref().map(|t| t.is_elapsed()).unwrap_or(true)
    }

    #[inline]
    pub fn poll_elapsed(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        self.hnd
            .as_ref()
            .map(|t| t.poll_elapsed(cx))
            .unwrap_or(Poll::Pending)
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

/// Interval returned by [`interval`]
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
    /// Create new sleep future
    #[inline]
    pub fn new(period: Millis) -> Interval {
        Interval {
            hnd: TimerHandle::new(period.0 as u64),
            period: period.0,
        }
    }

    #[inline]
    pub async fn tick(&self) {
        poll_fn(|cx| self.poll_tick(cx)).await;
    }

    #[inline]
    pub fn poll_tick(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        if self.hnd.poll_elapsed(cx).is_ready() {
            self.hnd.reset(self.period as u64);
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl crate::Stream for Interval {
    type Item = ();

    #[inline]
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.poll_tick(cx).map(|_| Some(()))
    }
}

#[cfg(test)]
#[allow(clippy::let_underscore_future)]
mod tests {
    use futures_util::StreamExt;
    use std::{future::poll_fn, rc::Rc, time};

    use super::*;
    use crate::future::lazy;

    /// State Under Test: Two calls of `now()` return the same value if they are done within resolution interval.
    ///
    /// Expected Behavior: Two back-to-back calls of `now()` return the same value.
    #[ntex_macros::rt_test2]
    async fn lowres_time_does_not_immediately_change() {
        let _ = sleep(Seconds(1));

        assert_eq!(now(), now())
    }

    /// State Under Test: `now()` updates returned value every ~1ms period.
    ///
    /// Expected Behavior: Two calls of `now()` made in subsequent resolution interval return different values
    /// and second value is greater than the first one at least by a 1ms interval.
    #[ntex_macros::rt_test2]
    async fn lowres_time_updates_after_resolution_interval() {
        let _ = sleep(Seconds(1));

        let first_time = now();

        sleep(Millis(25)).await;

        let second_time = now();
        assert!(second_time - first_time >= time::Duration::from_millis(25));
    }

    /// State Under Test: Two calls of `system_time()` return the same value if they are done within 1ms interval.
    ///
    /// Expected Behavior: Two back-to-back calls of `now()` return the same value.
    #[ntex_macros::rt_test2]
    async fn system_time_service_time_does_not_immediately_change() {
        let _ = sleep(Seconds(1));

        assert_eq!(system_time(), system_time());
        assert_eq!(system_time(), query_system_time());
    }

    /// State Under Test: `system_time()` updates returned value every 1ms period.
    ///
    /// Expected Behavior: Two calls of `system_time()` made in subsequent resolution interval return different values
    /// and second value is greater than the first one at least by a resolution interval.
    #[ntex_macros::rt_test2]
    async fn system_time_service_time_updates_after_resolution_interval() {
        let _ = sleep(Seconds(1));

        let wait_time = 300;

        let first_time = system_time()
            .duration_since(time::SystemTime::UNIX_EPOCH)
            .unwrap();

        sleep(Millis(wait_time)).await;

        let second_time = system_time()
            .duration_since(time::SystemTime::UNIX_EPOCH)
            .unwrap();

        assert!(second_time - first_time >= time::Duration::from_millis(wait_time as u64));
    }

    #[ntex_macros::rt_test2]
    async fn test_sleep_0() {
        let _ = sleep(Seconds(1));

        let first_time = now();
        sleep(Millis(0)).await;
        let second_time = now();
        assert!(second_time - first_time >= time::Duration::from_millis(1));

        let first_time = now();
        sleep(Millis(1)).await;
        let second_time = now();
        assert!(second_time - first_time >= time::Duration::from_millis(1));

        let first_time = now();
        let fut = sleep(Millis(10000));
        assert!(!fut.is_elapsed());
        fut.reset(Millis::ZERO);
        fut.await;
        let second_time = now();
        assert!(second_time - first_time < time::Duration::from_millis(1));

        let first_time = now();
        let fut = Sleep {
            hnd: TimerHandle::new(0),
        };
        assert!(fut.is_elapsed());
        fut.await;
        let second_time = now();
        assert!(second_time - first_time < time::Duration::from_millis(1));

        let first_time = now();
        let fut = Rc::new(sleep(Millis(100000)));
        let s = fut.clone();
        ntex::rt::spawn(async move {
            s.elapse();
        });
        poll_fn(|cx| fut.poll_elapsed(cx)).await;
        assert!(fut.is_elapsed());
        let second_time = now();
        assert!(second_time - first_time < time::Duration::from_millis(1));
    }

    #[ntex_macros::rt_test2]
    async fn test_deadline() {
        let _ = sleep(Seconds(1));

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
        assert!(dl.is_elapsed());
        dl.reset(Millis(1));
        assert!(lazy(|cx| dl.poll_elapsed(cx)).await.is_pending());

        assert!(format!("{:?}", dl).contains("Deadline"));
    }

    #[ntex_macros::rt_test2]
    async fn test_interval() {
        let mut int = interval(Millis(250));

        let time = time::Instant::now();
        int.tick().await;
        let elapsed = time::Instant::now() - time;
        assert!(
            elapsed > time::Duration::from_millis(200)
                && elapsed < time::Duration::from_millis(450),
            "elapsed: {:?}",
            elapsed
        );

        let time = time::Instant::now();
        int.next().await;
        let elapsed = time::Instant::now() - time;
        assert!(
            elapsed > time::Duration::from_millis(200)
                && elapsed < time::Duration::from_millis(450),
            "elapsed: {:?}",
            elapsed
        );
    }

    #[ntex_macros::rt_test2]
    async fn test_interval_one_sec() {
        let int = interval(Millis::ONE_SEC);

        for _i in 0..3 {
            let time = time::Instant::now();
            int.tick().await;
            let elapsed = time::Instant::now() - time;
            assert!(
                elapsed > time::Duration::from_millis(1000)
                    && elapsed < time::Duration::from_millis(1300),
                "elapsed: {:?}",
                elapsed
            );
        }
    }

    #[ntex_macros::rt_test2]
    async fn test_timeout_checked() {
        let result = timeout_checked(Millis(200), sleep(Millis(100))).await;
        assert!(result.is_ok());

        let result = timeout_checked(Millis(5), sleep(Millis(100))).await;
        assert!(result.is_err());

        let result = timeout_checked(Millis(0), sleep(Millis(100))).await;
        assert!(result.is_ok());
    }
}
