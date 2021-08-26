/// Utilities for tracking time.
use std::{future::Future, pin::Pin, task, task::Poll, time::Duration, time::Instant};
use tokio::time;

pub use tokio::time::{interval, Interval};
pub use tokio::time::{timeout, Timeout};

/// Waits until `deadline` is reached.
///
/// No work is performed while awaiting on the sleep future to complete. `Sleep`
/// operates at millisecond granularity and should not be used for tasks that
/// require high-resolution timers.
///
/// # Cancellation
///
/// Canceling a sleep instance is done by dropping the returned future. No additional
/// cleanup work is required.
#[inline]
pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep {
        inner: time::sleep_until(deadline.into()),
    }
}

/// Waits until `duration` has elapsed.
///
/// Equivalent to `sleep_until(Instant::now() + duration)`. An asynchronous
/// analog to `std::thread::sleep`.
pub fn sleep(duration: Duration) -> Sleep {
    Sleep {
        inner: time::sleep(duration),
    }
}

/// Creates new [`Interval`] that yields with interval of `period` with the
/// first tick completing at `start`. The default [`MissedTickBehavior`] is
/// [`Burst`](MissedTickBehavior::Burst), but this can be configured
/// by calling [`set_missed_tick_behavior`](Interval::set_missed_tick_behavior).
#[inline]
pub fn interval_at(start: Instant, period: Duration) -> Interval {
    time::interval_at(start.into(), period)
}

pin_project_lite::pin_project! {
    /// Future returned by [`sleep`](sleep) and [`sleep_until`](sleep_until).
    #[derive(Debug)]
    pub struct Sleep {
        #[pin]
        inner: time::Sleep,
    }
}

impl Sleep {
    /// Returns the instant at which the future will complete.
    #[inline]
    pub fn deadline(&self) -> Instant {
        self.inner.deadline().into_std()
    }

    /// Returns `true` if `Sleep` has elapsed.
    ///
    /// A `Sleep` instance is elapsed when the requested duration has elapsed.
    #[inline]
    pub fn is_elapsed(&self) -> bool {
        self.inner.is_elapsed()
    }

    /// Resets the `Sleep` instance to a new deadline.
    ///
    /// Calling this function allows changing the instant at which the `Sleep`
    /// future completes without having to create new associated state.
    ///
    /// This function can be called both before and after the future has
    /// completed.
    #[inline]
    pub fn reset(self: Pin<&mut Self>, deadline: Instant) {
        self.project().inner.reset(deadline.into());
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}
