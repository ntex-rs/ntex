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
mod tests {
    use super::*;

    static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[allow(clippy::cast_possible_truncation)]
    fn gettid() -> i32 {
        unsafe { libc::syscall(libc::SYS_gettid) as i32 }
    }

    #[test]
    fn capture_slot() {
        let _guard = CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tid = gettid();

        assert!(CAPTURE.arm(tid));
        assert!(!CAPTURE.arm(tid + 1));

        // signal for another thread is ignored
        CAPTURE.capture(tid + 1, file!());
        assert!(CAPTURE.take(tid).is_none());
        assert!(CAPTURE.take(tid + 1).is_none());

        CAPTURE.capture(tid, file!());
        assert!(CAPTURE.take(tid + 1).is_none());
        assert!(CAPTURE.take(tid).is_some());
        assert!(CAPTURE.take(tid).is_none());
        CAPTURE.disarm(tid);

        // late signal after disarm is ignored
        assert!(CAPTURE.arm(tid));
        CAPTURE.disarm(tid);
        CAPTURE.capture(tid, file!());
        assert!(CAPTURE.take(tid).is_none());

        // ready but not taken capture is released by disarm
        assert!(CAPTURE.arm(tid));
        CAPTURE.capture(tid, file!());
        CAPTURE.disarm(tid);
        assert!(CAPTURE.arm(tid));
        CAPTURE.disarm(tid);
    }

    #[test]
    fn capture_from_signal_handler() {
        let _guard = CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = unsafe {
            signal_hook::low_level::register(signal_hook::consts::SIGUSR2, crate::system::sig_usr2)
                .unwrap()
        };
        let tid = gettid();

        assert!(CAPTURE.arm(tid));
        let res = unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGUSR2) };
        assert_eq!(res, 0);
        let bt = CAPTURE
            .take(tid)
            .expect("backtrace captured in signal handler");
        CAPTURE.disarm(tid);
        signal_hook::low_level::unregister(id);

        let bt = ntex_error::Backtrace::from(bt);
        drop(bt.resolver().resolve());
        assert!(bt.is_resolved());
    }
}
