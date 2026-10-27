//! Backtrace capture slot for the SIGUSR2 handler.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{cell::UnsafeCell, mem::MaybeUninit};

pub(crate) static CAPTURE: Capture = Capture::new();

/// Single backtrace slot shared between the ping loop and the SIGUSR2 handler.
///
/// The handler only uses atomics and writes into a preallocated slot, it never
/// locks or allocates. State is `tid << 3 | tag`, so a late signal cannot fill
/// a slot armed for another thread.
pub(crate) struct Capture {
    state: AtomicUsize,
    slot: UnsafeCell<MaybeUninit<ntex_error::BacktraceRaw>>,
}

unsafe impl Sync for Capture {}

impl Capture {
    const IDLE: usize = 0;
    const ARMED: usize = 1;
    const WRITING: usize = 2;
    const READY: usize = 3;
    const ABANDONED: usize = 4;

    const fn new() -> Self {
        Self {
            state: AtomicUsize::new(Self::IDLE),
            slot: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    #[allow(clippy::cast_sign_loss)]
    fn state(tid: i32, tag: usize) -> usize {
        ((tid as u32 as usize) << 3) | tag
    }

    /// Arm the slot for `tid`, fails if another capture is in progress.
    pub(crate) fn arm(&self, tid: i32) -> bool {
        self.state
            .compare_exchange(
                Self::IDLE,
                Self::state(tid, Self::ARMED),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Called from the signal handler.
    pub(crate) fn capture(&self, tid: i32, location: &'static str) {
        if self
            .state
            .compare_exchange(
                Self::state(tid, Self::ARMED),
                Self::state(tid, Self::WRITING),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return;
        }

        // Frame walking only, symbol resolution happens on the system thread.
        let bt = unsafe { ntex_error::BacktraceRaw::with_filename_unsynchronized(location) };
        unsafe { (*self.slot.get()).write(bt) };

        if self
            .state
            .compare_exchange(
                Self::state(tid, Self::WRITING),
                Self::state(tid, Self::READY),
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_err()
        {
            // abandoned by the ping loop
            unsafe { (*self.slot.get()).assume_init_drop() };
            self.state.store(Self::IDLE, Ordering::Release);
        }
    }

    pub(crate) fn take(&self, tid: i32) -> Option<ntex_error::BacktraceRaw> {
        if self.state.load(Ordering::Acquire) == Self::state(tid, Self::READY) {
            let bt = unsafe { (*self.slot.get()).assume_init_read() };
            self.state.store(Self::IDLE, Ordering::Release);
            Some(bt)
        } else {
            None
        }
    }

    pub(crate) fn disarm(&self, tid: i32) {
        loop {
            let cur = self.state.load(Ordering::Acquire);
            let next = if cur == Self::state(tid, Self::ARMED) {
                Self::IDLE
            } else if cur == Self::state(tid, Self::WRITING) {
                // handler is still running, it releases the slot when done
                Self::state(tid, Self::ABANDONED)
            } else if cur == Self::state(tid, Self::READY) {
                let _ = self.take(tid);
                return;
            } else {
                return;
            };
            if self
                .state
                .compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/capture.rs"]
mod tests;
