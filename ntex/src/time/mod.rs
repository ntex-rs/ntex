//! Utilities for tracking time.

use std::{future::Future, pin::Pin, task, task::Poll};

mod types;
mod wheel;

pub use self::types::{Millis, Seconds};
pub use self::wheel::{now, system_time, TimerHandle};

/// Waits until `duration` has elapsed.
///
/// No work is performed while awaiting on the sleep future to complete. `Sleep`
/// operates at 16 millisecond granularity and should not be used for tasks that
/// require high-resolution timers.
#[inline]
pub fn sleep<T: Into<Millis>>(dur: T) -> Sleep {
    Sleep::new(dur.into())
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

/// Future returned by [`sleep`](sleep).
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
            hnd: TimerHandle::new(duration.0),
        }
    }

    /// Returns `true` if `Sleep` has elapsed.
    #[inline]
    pub fn is_elapsed(&self) -> bool {
        self.hnd.is_elapsed()
    }

    /// Resets the `Sleep` instance to a new deadline.
    ///
    /// Calling this function allows changing the instant at which the `Sleep`
    /// future completes without having to create new associated state.
    ///
    /// This function can be called both before and after the future has
    /// completed.
    pub fn reset<T: Into<Millis>>(&self, millis: T) {
        self.hnd.reset(millis.into().0);
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

/// Interval returned by [`interval`]
///
/// This type allows you to wait on a sequence of instants with a certain
/// duration between each instant.
#[derive(Debug)]
pub struct Interval {
    hnd: TimerHandle,
    period: u64,
}

impl Interval {
    /// Create new sleep future
    #[inline]
    pub fn new(period: Millis) -> Interval {
        Interval {
            hnd: TimerHandle::new(period.0),
            period: period.0,
        }
    }

    #[inline]
    pub async fn tick(&self) {
        crate::util::poll_fn(|cx| self.poll_tick(cx)).await;
    }

    #[inline]
    pub fn poll_tick(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        if self.hnd.poll_elapsed(cx).is_ready() {
            self.hnd.reset(self.period);
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
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time;

    /// State Under Test: Two calls of `now()` return the same value if they are done within resolution interval.
    ///
    /// Expected Behavior: Two back-to-back calls of `now()` return the same value.
    #[crate::rt_test]
    async fn lowres_time_does_not_immediately_change() {
        assert_eq!(now(), now());
    }

    /// State Under Test: `now()` updates returned value every ~1ms period.
    ///
    /// Expected Behavior: Two calls of `now()` made in subsequent resolution interval return different values
    /// and second value is greater than the first one at least by a 1ms interval.
    #[crate::rt_test]
    async fn lowres_time_updates_after_resolution_interval() {
        let first_time = now();

        sleep(Millis(25)).await;

        let second_time = now();
        assert!(second_time - first_time >= time::Duration::from_millis(25));
    }

    /// State Under Test: Two calls of `system_time()` return the same value if they are done within 1ms interval.
    ///
    /// Expected Behavior: Two back-to-back calls of `now()` return the same value.
    #[crate::rt_test]
    async fn system_time_service_time_does_not_immediately_change() {
        assert_eq!(system_time(), system_time());
    }

    /// State Under Test: `system_time()` updates returned value every 1ms period.
    ///
    /// Expected Behavior: Two calls of `system_time()` made in subsequent resolution interval return different values
    /// and second value is greater than the first one at least by a resolution interval.
    #[crate::rt_test]
    async fn system_time_service_time_updates_after_resolution_interval() {
        let wait_time = 300;

        let first_time = system_time()
            .duration_since(time::SystemTime::UNIX_EPOCH)
            .unwrap();

        sleep(Millis(wait_time)).await;

        let second_time = system_time()
            .duration_since(time::SystemTime::UNIX_EPOCH)
            .unwrap();

        assert!(second_time - first_time >= time::Duration::from_millis(wait_time));
    }

    #[crate::rt_test]
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

    #[crate::rt_test]
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
}
