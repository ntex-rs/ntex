//! Driver tasks of the thread-local wheel, used without a native runtime.
use std::time::Instant;
use std::{future::Future, pin::Pin, rc::Rc, task, task::Poll};

use super::{Flags, LOWRES_RESOLUTION, Processed, TIMER, Timer};

/// Sleep of the driver tasks, backed by the Tokio timer with the `tokio`
/// feature.
///
/// The Tokio sleep is created on the first poll, inside the driver task, so
/// the driver can be built outside of the runtime context.
#[cfg(feature = "tokio")]
pub(super) struct Sleep {
    deadline: Instant,
    inner: Option<Pin<Box<tok_io::time::Sleep>>>,
}

#[cfg(feature = "tokio")]
impl Sleep {
    pub(super) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            inner: None,
        }
    }

    fn reset(&mut self, deadline: Instant) {
        self.deadline = deadline;
        if let Some(inner) = &mut self.inner {
            inner.as_mut().reset(deadline.into());
        }
    }

    fn poll(&mut self, cx: &mut task::Context<'_>) -> Poll<()> {
        let deadline = self.deadline;
        self.inner
            .get_or_insert_with(|| Box::pin(tok_io::time::sleep_until(deadline.into())))
            .as_mut()
            .poll(cx)
    }
}

/// Sleep of the driver tasks, backed by `futures-timer` without the `tokio`
/// feature.
#[cfg(not(feature = "tokio"))]
pub(super) struct Sleep(futures_timer::Delay);

#[cfg(not(feature = "tokio"))]
impl Sleep {
    pub(super) fn new(deadline: Instant) -> Self {
        Self(futures_timer::Delay::new(
            deadline.saturating_duration_since(Instant::now()),
        ))
    }

    fn reset(&mut self, deadline: Instant) {
        self.0
            .reset(deadline.saturating_duration_since(Instant::now()));
    }

    fn poll(&mut self, cx: &mut task::Context<'_>) -> Poll<()> {
        Pin::new(&mut self.0).poll(cx)
    }
}

/// Resets the timer when dropped with the arbiter storage of a stopped system.
#[derive(Default)]
pub(super) struct TimerReset;

impl TimerReset {
    fn register() {
        crate::with_item::<TimerReset, _, _>(|_| ());
    }
}

impl Drop for TimerReset {
    fn drop(&mut self) {
        // the timer is already destroyed if the thread is exiting
        let _ = TIMER.try_with(|t| t.reset());
    }
}

/// Task that sleeps until the next bucket expires and wakes its timers.
pub(super) struct TimerDriver {
    pub(super) timer: Rc<Timer>,
    pub(super) generation: u64,
    pub(super) sleep: Sleep,
    /// Deadline the sleep is armed for.
    pub(super) armed: Option<Instant>,
}

impl TimerDriver {
    pub(super) fn start(timer: &Rc<Timer>) {
        timer.insert_flags(Flags::DRIVER_STARTED);
        TimerReset::register();

        let deadline = timer.expiry_time(timer.next_expiry.get());
        crate::spawn_detached(TimerDriver {
            timer: timer.clone(),
            generation: timer.generation.get(),
            sleep: Sleep::new(deadline),
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
        if this.timer.generation.get() != this.generation {
            return Poll::Ready(());
        }
        this.timer.driver.register(cx.waker());

        let mut now = Instant::now();
        loop {
            match this.timer.process(now, this.generation) {
                // the wheel is empty, a new timer wakes the driver
                Processed::Idle => return Poll::Pending,
                Processed::Stopped => return Poll::Ready(()),
                Processed::Next(deadline) => {
                    if this.armed != Some(deadline) {
                        this.armed = Some(deadline);
                        this.sleep.reset(deadline);
                    }
                    if this.sleep.poll(cx).is_pending() {
                        return Poll::Pending;
                    }
                    // the sleep fired, it is re-armed if it fired early
                    this.armed = None;
                    now = Instant::now();
                }
            }
        }
    }
}

/// Task that invalidates the cached time every `LOWRES_RESOLUTION`.
pub(super) struct LowresTimerDriver {
    pub(super) timer: Rc<Timer>,
    pub(super) generation: u64,
    pub(super) sleep: Sleep,
}

impl LowresTimerDriver {
    pub(super) fn start(timer: &Rc<Timer>) {
        timer.insert_flags(Flags::LOWRES_DRIVER);
        TimerReset::register();

        crate::spawn_detached(LowresTimerDriver {
            timer: timer.clone(),
            generation: timer.generation.get(),
            sleep: Sleep::new(Instant::now() + LOWRES_RESOLUTION),
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
            this.sleep.reset(Instant::now() + LOWRES_RESOLUTION);
        }

        if this.sleep.poll(cx).is_ready() {
            timer.lowres_stime.set(None);
            flags.remove(Flags::LOWRES_TIMER);
            timer.flags.set(flags);
        }
        Poll::Pending
    }
}
