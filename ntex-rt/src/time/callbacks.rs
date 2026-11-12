use std::num::NonZeroUsize;
use std::{cell::Cell, cmp, fmt, rc::Rc, rc::Weak};

use crate::HashMap;

use super::{TIMER, TimerEntry};

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
pub struct CallbackHandle {
    pub(super) hnd: NonZeroUsize,
    pub(super) state: Rc<CallbackState>,
}

impl CallbackHandle {
    #[allow(clippy::missing_panics_doc)] // unwraps only check wheel invariants
    /// Arms a callback after `millis`, using the wheel's rounding and limits.
    ///
    /// The base granularity is approximately 16 ms by default, see
    /// [`Builder::timer_resolution`](crate::Builder::timer_resolution);
    /// longer delays use coarser rounding, and delays beyond the wheel's
    /// capacity (about 24 days at 16 ms) are clamped. Callback delivery may be
    /// delayed further while the runtime is busy.
    ///
    /// A zero delay waits for a subsequent timer tick; the callback is never
    /// invoked inline.
    pub fn new(millis: u64, callback: impl Fn() + 'static) -> Self {
        let state = Rc::new(CallbackState {
            callback: Box::new(callback),
            arm: Cell::new(0),
            pending: Cell::new(true),
        });
        TIMER.with(|timer| {
            let (idx, expiry) = timer.calc_bucket(cmp::max(millis, 1));
            let no = timer.with_wheel(|wheel| {
                let no = wheel.timers.insert(TimerEntry {
                    bucket: None,
                    bucket_entry: 0,
                    callback: true,
                    task: None,
                });
                wheel
                    .callbacks
                    .get_or_insert_with(|| Box::new(Callbacks::default()))
                    .entries
                    .insert(
                        no,
                        CallbackTicket {
                            state: Rc::downgrade(&state),
                            arm: 0,
                            generation: timer.generation.get(),
                        },
                    );
                wheel.link(no, idx);
                no
            });
            let handle = Self {
                hnd: NonZeroUsize::new(no).unwrap(),
                state,
            };
            timer.update_next_expiry(expiry);
            handle
        })
    }

    #[allow(clippy::missing_panics_doc)] // unwraps only check wheel invariants
    /// Arms another invocation, invalidating any previous pending delivery.
    ///
    /// This also suppresses an expired invocation that has been queued but has
    /// not started. It can be called before or after expiration or cancellation.
    ///
    /// A zero delay waits for a subsequent tick. Resetting from within the
    /// callback arms a new invocation, which survives completion of the old one.
    pub fn reset(&self, millis: u64) {
        TIMER.with(|timer| {
            let (idx, expiry) = timer.calc_bucket(cmp::max(millis, 1));
            timer.with_wheel(|wheel| {
                let ticket = wheel
                    .callbacks
                    .as_mut()
                    .unwrap()
                    .entries
                    .get_mut(&self.hnd.get())
                    .unwrap();
                ticket.arm = ticket.arm.wrapping_add(1);
                ticket.generation = timer.generation.get();
                self.state.arm.set(ticket.arm);
                self.state.pending.set(true);
                wheel.relink(self.hnd.get(), idx);
            });
            timer.update_next_expiry(expiry);
        });
    }

    /// Disarms the timer and suppresses pending delivery, retaining the callback.
    ///
    /// This also suppresses an expired invocation that has not started. Calling
    /// this method on an already disarmed timer is harmless.
    ///
    /// An invocation that has already started cannot be interrupted.
    pub fn cancel(&self) {
        self.state.pending.set(false);
        TIMER.with(|timer| timer.with_wheel(|wheel| wheel.unlink(self.hnd.get())));
    }

    /// Returns `true` when disarmed, including after cancellation or shutdown.
    ///
    /// Expiration marks the timer elapsed before callback delivery. This query
    /// does not indicate whether the callback has run.
    pub fn is_elapsed(&self) -> bool {
        TIMER.with(|timer| timer.with_wheel(|wheel| wheel.timers[self.hnd.get()].bucket.is_none()))
    }
}

impl fmt::Debug for CallbackHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallbackHandle")
            .field("handle", &self.hnd)
            .finish_non_exhaustive()
    }
}

impl Drop for CallbackHandle {
    fn drop(&mut self) {
        self.state.pending.set(false);
        let _ = TIMER.try_with(|timer| {
            timer.with_wheel(|wheel| {
                wheel.unlink(self.hnd.get());
                wheel.timers.remove(self.hnd.get());
                wheel
                    .callbacks
                    .as_mut()
                    .unwrap()
                    .entries
                    .remove(&self.hnd.get());
            });
        });
    }
}

pub(super) struct CallbackState {
    pub(super) callback: Box<dyn Fn()>,
    pub(super) arm: Cell<u64>,
    pub(super) pending: Cell<bool>,
}

#[derive(Clone)]
pub(super) struct CallbackTicket {
    // Weak identity prevents slab reuse from redirecting a delivery and lets
    // handle drop release captures without waiting for the queued ticket.
    pub(super) state: Weak<CallbackState>,
    pub(super) arm: u64,
    pub(super) generation: u64,
}

#[derive(Default)]
pub(super) struct Callbacks {
    pub(super) entries: HashMap<usize, CallbackTicket>,
    pub(super) ready: Vec<CallbackTicket>,
}
