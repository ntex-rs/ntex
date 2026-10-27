use super::*;

static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[allow(clippy::cast_possible_truncation)]
fn gettid() -> i32 {
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// STATEMENT: A capture slot accepts only its armed thread and releases pending captures on disarm.
///
/// HOW:
/// - Try competing arms, mismatched captures and reads, and repeated retrieval.
/// - Disarm before and after capture, then verify late signals are ignored and rearming succeeds.
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

/// STATEMENT: The watchdog signal handler captures a resolvable backtrace for the armed thread.
///
/// HOW:
/// - Register the handler, arm the current thread, and deliver SIGUSR2 to that thread.
/// - Take and resolve the captured backtrace after releasing the handler and slot.
#[test]
fn capture_from_signal_handler() {
    let _guard = CAPTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = unsafe {
        signal_hook::low_level::register(signal_hook::consts::SIGUSR2, crate::watchdog::sig_usr2)
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
