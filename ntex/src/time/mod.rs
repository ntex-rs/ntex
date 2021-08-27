//! Utilities for tracking time.

use std::{convert::TryInto, future::Future, pin::Pin, task, task::Poll, time};

mod wheel;

pub use self::wheel::TimerHandle;

/// Waits until `duration` has elapsed.
///
/// No work is performed while awaiting on the sleep future to complete. `Sleep`
/// operates at 16.5 millisecond granularity and should not be used for tasks that
/// require high-resolution timers.
#[inline]
pub fn sleep(millis: u64) -> Sleep {
    Sleep::new(millis)
}

/// Waits until `duration` has elapsed.
///
/// No work is performed while awaiting on the sleep future to complete. `Sleep`
/// operates at 16.5 millisecond granularity and should not be used for tasks that
/// require high-resolution timers.
#[inline]
pub fn sleep_duration(duration: time::Duration) -> Sleep {
    Sleep::new(duration.as_millis().try_into().unwrap_or_else(|_| {
        log::error!("Duration is too large {:?}", duration);
        1 << 31
    }))
}

/// Require a `Future` to complete before the specified duration has elapsed.
///
/// If the future completes before the duration has elapsed, then the completed
/// value is returned. Otherwise, an error is returned and the future is
/// canceled.
pub fn timeout<T>(millis: u64, future: T) -> Timeout<T>
where
    T: Future,
{
    Timeout::new_with_delay(future, Sleep::new(millis))
}

/// Future returned by [`sleep`](sleep).
///
/// # Examples
///
/// Wait 100ms and print "100 ms have elapsed".
///
/// ```
/// use ntex::time::sleep;
///
/// #[ntex::main]
/// async fn main() {
///     sleep(100).await;
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
    pub fn new(millis: u64) -> Sleep {
        Sleep {
            hnd: TimerHandle::new(millis),
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
    pub fn reset(&self, millis: u64) {
        self.hnd.reset(millis);
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
