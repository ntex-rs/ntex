use crate::testing::TestRunner;

use super::*;

// signal handling is global
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn recv(rx: oneshot::AsyncReceiver<Arc<[Signal]>>) -> Option<Arc<[Signal]>> {
    let mut rx = std::pin::pin!(rx);
    let mut timeout = std::pin::pin!(futures_timer::Delay::new(std::time::Duration::from_secs(5)));
    poll_fn(|cx| {
        if let Poll::Ready(res) = rx.as_mut().poll(cx) {
            Poll::Ready(res.ok())
        } else if timeout.as_mut().poll(cx).is_ready() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    })
    .await
}

/// Raises a signal with `raise` until a batch with a signal matching
/// `expected` is delivered, returns the number of attempts.
///
/// The panic hook is process-wide, panics of tests running in parallel are
/// delivered too. A batch without the expected signal consumes the
/// handler, and the expected signal may be dropped before the handler is
/// registered again, so it is raised again.
async fn deliver(raise: impl Fn(), expected: impl Fn(&Signal) -> bool) -> usize {
    for attempt in 1..=10 {
        let rx = signal();
        // let the registration task run
        futures_timer::Delay::new(std::time::Duration::from_millis(50)).await;
        raise();
        if recv(rx)
            .await
            .is_some_and(|sigs| sigs.iter().any(&expected))
        {
            return attempt;
        }
    }
    panic!("signal is not delivered");
}

fn is_app_panic(sig: &Signal, expected: &str) -> bool {
    matches!(sig, Signal::Panic(PanicSource::App(msg, _)) if &**msg == expected)
}

/// STATEMENT: Panic handling installs once, chains the prior hook, and respects signal ownership.
///
/// HOW:
/// - Enable the hook twice and panic with signal handling disabled, checking only the prior hook.
/// - Enable signals and verify panic delivery invokes the prior hook once per attempt.
#[test]
fn panic_hook_chains_and_follows_signals() {
    use std::sync::atomic::AtomicUsize;

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // panics of other tests reach the hook too
        let msg = info.payload().downcast_ref::<&str>();
        if matches!(msg, Some(&("no signals" | "boom"))) {
            CALLS.fetch_add(1, Ordering::Relaxed);
        }
        prev(info);
    }));
    enable_panic_handling();
    // installed once
    enable_panic_handling();

    // previous hook is called, nothing is queued without signal handling
    SIGS.lock().clear();
    let _ = panic::catch_unwind(|| panic!("no signals"));
    assert_eq!(CALLS.load(Ordering::Relaxed), 1);
    assert!(
        !SIGS
            .lock()
            .iter()
            .any(|sig| is_app_panic(sig, "no signals"))
    );

    let attempts = System::new("test", TestRunner).block_on(async {
        let sys = System::current();
        sys.enable_signals();

        deliver(
            || {
                let _ = panic::catch_unwind(|| panic!("boom"));
            },
            |sig| is_app_panic(sig, "boom"),
        )
        .await
    });
    // the hook is installed once and chains the previous one
    assert_eq!(CALLS.load(Ordering::Relaxed), 1 + attempts);
}

/// STATEMENT: Panics on other threads are delivered to the system as application signals.
///
/// HOW:
/// - Enable panic and signal handling, then panic on a spawned thread.
/// - Await a signal carrying that thread's panic message.
#[test]
fn panic_on_other_thread_delivered() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    enable_panic_handling();

    System::new("test", TestRunner).block_on(async {
        System::current().enable_signals();

        // compio aborts the process if the task is woken on a panicking thread
        deliver(
            || {
                let res = std::thread::spawn(|| panic!("thread boom")).join();
                assert!(res.is_err());
            },
            |sig| is_app_panic(sig, "thread boom"),
        )
        .await;
    });
}

/// STATEMENT: A panic exiting a system releases its global signal ownership.
///
/// HOW:
/// - Panic from `block_on` and from `run` initialization while signals are enabled.
/// - Catch each panic and verify signal handling is disabled afterward.
#[test]
fn signals_released_when_future_panics() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let res = panic::catch_unwind(|| {
        System::new("block_on", TestRunner).block_on(async {
            System::current().enable_signals();
            assert!(is_enabled());
            panic!("block_on fails");
        });
    });
    assert!(res.is_err());
    assert!(!is_enabled());

    let res = panic::catch_unwind(|| {
        System::build().signals(true).build(TestRunner).run(|| {
            assert!(is_enabled());
            panic!("run fails");
        })
    });
    assert!(res.is_err());
    assert!(!is_enabled());
}

/// STATEMENT: Stopped systems release signals so a later system can acquire them exclusively.
///
/// HOW:
/// - Finish one signal-owning system, then enable signals on its successor.
/// - Attempt acquisition from a concurrent system and verify only the successor owns signals.
#[test]
fn signals_released_when_system_stops() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let first = System::new("first", TestRunner).block_on(async {
        let sys = System::current();
        sys.enable_signals();
        assert!(sys.signals());
        sys
    });
    assert!(!is_enabled());
    assert!(!first.signals());

    System::new("second", TestRunner).block_on(async {
        let sys = System::current();
        sys.enable_signals();
        assert!(sys.signals());
        assert!(is_enabled());

        // signals are handled by one system at a time
        let other = std::thread::spawn(|| {
            System::new("other", TestRunner).block_on(async {
                let sys = System::current();
                sys.enable_signals();
                sys.signals()
            })
        })
        .join()
        .unwrap();
        assert!(!other);
        assert!(is_enabled());
    });
    assert!(!is_enabled());
}

/// STATEMENT: Builder-enabled signal handling is released after stop or initialization failure.
///
/// HOW:
/// - Stop a signal-enabled system from another thread and verify ownership is released.
/// - Fail initialization in another system and check its error and released signal state.
#[test]
fn builder_signals() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // stopped by another thread
    let stopper = std::thread::spawn(|| {
        loop {
            let sys = CUR_SYS.lock().clone().filter(|sys| sys.name() == "signals");
            if let Some(sys) = sys {
                sys.stop();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    System::build()
        .name("signals")
        .enable_signals()
        .build(TestRunner)
        .run_until_stop()
        .unwrap();
    stopper.join().unwrap();
    assert!(!is_enabled());

    // signals are released if `f` fails
    let err = System::build()
        .signals(true)
        .build(TestRunner)
        .run(|| {
            assert!(System::current().signals());
            assert!(is_enabled());
            Err(std::io::Error::other("failed"))
        })
        .unwrap_err();
    assert_eq!(err.to_string(), "failed");
    assert!(!is_enabled());
}

/// STATEMENT: Builder-configured panic handling delivers string payloads in both runner modes.
///
/// HOW:
/// - Receive a thread panic with an owned string through `run` and a string slice through `block_on`.
/// - Verify both modes release signal ownership on exit.
#[test]
fn builder_panic_handling() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    System::build()
        .signals(true)
        .panic_handling(true)
        .build(TestRunner)
        .run(|| {
            crate::spawn_detached(async {
                deliver(
                    || {
                        // a `String` payload, like a formatted message
                        let res = std::thread::spawn(|| panic::panic_any("run".to_string())).join();
                        assert!(res.is_err());
                    },
                    |sig| is_app_panic(sig, "run"),
                )
                .await;
                System::current().stop();
            });
            Ok(())
        })
        .unwrap();
    assert!(!is_enabled());

    System::build()
        .signals(true)
        .panic_handling(true)
        .build(TestRunner)
        .block_on(async {
            assert!(is_enabled());
            deliver(
                || {
                    let res = std::thread::spawn(|| panic!("block_on")).join();
                    assert!(res.is_err());
                },
                |sig| is_app_panic(sig, "block_on"),
            )
            .await;
        });
    assert!(!is_enabled());
}

/// STATEMENT: Only one system can acquire global signal handling concurrently.
///
/// HOW:
/// - Synchronize four systems to attempt registration before any owner exits.
/// - Verify exactly one succeeds and ownership is released after all finish.
#[test]
fn signals_registered_by_one_system() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let barrier = Arc::new(std::sync::Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                System::new(&format!("sys{i}"), TestRunner).block_on(async move {
                    let sys = System::current();
                    barrier.wait();
                    sys.enable_signals();
                    let enabled = sys.signals();
                    // keep signals registered until all systems tried
                    barrier.wait();
                    enabled
                })
            })
        })
        .collect();
    let enabled = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|enabled| *enabled)
        .count();
    assert_eq!(enabled, 1);
    assert!(!is_enabled());
}

/// STATEMENT: Concurrent signal producers do not lose queued signals.
///
/// HOW:
/// - Queue one hundred hangup signals from four threads while draining the shared queue.
/// - Join the producers and verify the total received count.
#[test]
fn signals_queued_from_many_threads() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    SIGS.lock().clear();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..25 {
                    handle_signal(Signal::Hup);
                }
            })
        })
        .collect();
    let hups = || {
        let sigs = std::mem::take(&mut *SIGS.lock());
        sigs.iter().filter(|sig| matches!(sig, Signal::Hup)).count()
    };
    let mut received = 0;
    while received < 100 && !handles.iter().all(std::thread::JoinHandle::is_finished) {
        received += hups();
    }
    for h in handles {
        h.join().unwrap();
    }
    received += hups();
    assert_eq!(received, 100);
}

#[cfg(target_family = "unix")]
/// STATEMENT: An operating-system hangup signal reaches the owning system.
///
/// HOW:
/// - Enable signals, send SIGHUP to the process, and await a hangup notification.
/// - Finish the system and confirm signal ownership is released.
#[test]
fn os_signal_delivered() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    System::new("test", TestRunner).block_on(async {
        let sys = System::current();
        sys.enable_signals();
        assert!(sys.signals());

        deliver(
            || unsafe {
                libc::kill(libc::getpid(), libc::SIGHUP);
            },
            |sig| matches!(sig, Signal::Hup),
        )
        .await;
    });
    assert!(!is_enabled());
}

#[cfg(target_family = "windows")]
/// STATEMENT: Windows signal handling can be disabled and reenabled by the same system.
///
/// HOW:
/// - Enable, disable, and reenable signals before checking ownership.
/// - Disable once more and verify the system no longer owns signals.
#[test]
fn reenable_signals() {
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    System::new("test", TestRunner).block_on(async {
        let sys = System::current();
        sys.enable_signals();
        sys.disable_signals();
        sys.enable_signals();
        assert!(sys.signals());
        sys.disable_signals();
        assert!(!sys.signals());
    });
}

#[cfg(target_os = "linux")]
fn signal_child_mode() -> Option<String> {
    let mode = std::env::var("NTEX_FATAL_SIGNAL_CHILD").ok()?;
    for (resource, limit) in [(libc::RLIMIT_CORE, 0), (libc::RLIMIT_CPU, 10)] {
        let limits = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        assert_eq!(unsafe { libc::setrlimit(resource, &raw const limits) }, 0);
    }
    Some(mode)
}

#[cfg(target_os = "linux")]
fn signal_child(test: &str, mode: &str) -> std::process::ExitStatus {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &format!("signals::tests::{test}")])
        .env("NTEX_FATAL_SIGNAL_CHILD", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{test} ({mode}) did not terminate");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
/// STATEMENT: Real aborts and memory faults still terminate signal-enabled processes.
///
/// HOW:
/// - Trigger aborts and protected-memory faults in child processes on owner and worker threads.
/// - Check termination by the expected signal, including while shared locks are held.
#[test]
fn fatal_signals_terminate_process() {
    use std::os::unix::process::ExitStatusExt;

    if let Some(mode) = signal_child_mode() {
        System::new("test", TestRunner).block_on(async {
            System::current().enable_signals();
            let other_thread = mode.starts_with("thread-");
            let crash = move || {
                if mode.ends_with("abort") {
                    std::process::abort();
                }
                unsafe {
                    let page = libc::mmap(
                        std::ptr::null_mut(),
                        4096,
                        libc::PROT_NONE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    );
                    assert_ne!(page, libc::MAP_FAILED);
                    page.cast::<u8>().write_volatile(1);
                }
            };
            if other_thread {
                let _stderr = std::io::stderr().lock();
                let _sigs = SIGS.lock();
                std::thread::spawn(crash).join().unwrap();
            } else {
                crash();
            }
        });
        return;
    }

    for mode in ["abort", "segv", "thread-abort", "thread-segv"] {
        let status = signal_child("fatal_signals_terminate_process", mode);
        let expected = if mode.ends_with("abort") {
            libc::SIGABRT
        } else {
            libc::SIGSEGV
        };
        assert_eq!(status.signal(), Some(expected), "{mode}: {status}");
    }
}

#[cfg(target_os = "linux")]
async fn wait_for_signal_handler() {
    poll_fn(|cx| {
        if HANDLERS.with(|handlers| handlers.borrow().is_empty()) {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
}

#[cfg(target_os = "linux")]
fn raise_fatal_signal(sig: i32) {
    std::thread::spawn(move || {
        nix::errno::Errno::EBUSY.set();
        assert_eq!(unsafe { libc::raise(sig) }, 0);
        assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EBUSY);
    })
    .join()
    .unwrap();
}

#[cfg(target_os = "linux")]
/// STATEMENT: Explicit fatal-signal notifications avoid deadlock and preserve errno.
///
/// HOW:
/// - In child processes, raise signals with shared locks held and with the notification pipe full.
/// - Check named notifications, preserved errno, and isolation of signals raised after fork.
#[test]
fn fatal_signal_notifications() {
    use std::os::fd::AsRawFd;

    if let Some(mode) = signal_child_mode() {
        System::new("test", TestRunner).block_on(async move {
            System::current().enable_signals();
            if mode == "full" {
                // Block the receiver and fill the pipe: the signal handler
                // must still return without waiting or changing errno.
                let _sigs = SIGS.lock();
                let (_, _, write) = FATAL_PIPE.get().unwrap();
                let fd = write.as_raw_fd();
                assert_eq!(unsafe { libc::write(fd, b"A".as_ptr().cast(), 1) }, 1);
                let bytes = [0u8; 1];
                while unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) } >= 0 {}
                assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EAGAIN);
                raise_fatal_signal(libc::SIGABRT);
                return;
            }

            if mode == "fork" {
                let rx = signal();
                wait_for_signal_handler().await;
                let pid = unsafe { libc::fork() };
                assert!(pid >= 0);
                if pid == 0 {
                    unsafe {
                        libc::alarm(2);
                        libc::raise(libc::SIGABRT);
                        libc::_exit(0);
                    }
                }
                let mut status = 0;
                assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
                assert_eq!(status, 0);
                raise_fatal_signal(libc::SIGSEGV);
                let sigs = recv(rx).await.unwrap();
                assert!(
                    sigs.iter()
                        .all(|sig| matches!(sig, Signal::Panic(PanicSource::Sig("SIGSEGV"))))
                );
                return;
            }

            for (sig, name) in [(libc::SIGABRT, "SIGABRT"), (libc::SIGSEGV, "SIGSEGV")] {
                let rx = signal();
                wait_for_signal_handler().await;
                {
                    let _stderr = std::io::stderr().lock();
                    let _sigs = SIGS.lock();
                    raise_fatal_signal(sig);
                }
                let sigs = recv(rx).await.expect("fatal signal was not delivered");
                assert!(
                    sigs.iter()
                        .any(|sig| matches!(sig, Signal::Panic(PanicSource::Sig(s)) if *s == name))
                );
            }
        });
        return;
    }

    for mode in ["notify", "full", "fork"] {
        let status = signal_child("fatal_signal_notifications", mode);
        assert!(status.success(), "{mode}: {status}");
    }
}

#[cfg(target_os = "linux")]
/// STATEMENT: A delivered fatal-signal notification stops a running server cleanly.
///
/// HOW:
/// - Start a server in a child process and raise an abort or segmentation signal.
/// - Await server completion and verify its stopped status and successful child exit.
#[test]
fn fatal_signal_stops_server() {
    if let Some(mode) = signal_child_mode() {
        ntex::rt::System::new("test", ntex::rt::DefaultRuntime).block_on(async move {
            let srv = ntex::server::build()
                .workers(1)
                .graceful_shutdown()
                .bind(
                    "test",
                    "127.0.0.1:0",
                    ntex::service::cfg::SharedCfg::default(),
                    async |()| async |_| Ok::<_, ()>(()),
                )
                .unwrap()
                .run();
            while srv.status() != ntex::server::ServerStatus::Ready {
                srv.status_changed().await;
            }
            futures_timer::Delay::new(std::time::Duration::from_millis(50)).await;
            raise_fatal_signal(if mode == "abort" {
                libc::SIGABRT
            } else {
                libc::SIGSEGV
            });
            let stopped = srv.clone();
            srv.await.unwrap();
            assert_eq!(stopped.status(), ntex::server::ServerStatus::Stopped);
        });
        return;
    }

    for mode in ["abort", "segv"] {
        let status = signal_child("fatal_signal_stops_server", mode);
        assert!(status.success(), "{mode}: {status}");
    }
}
