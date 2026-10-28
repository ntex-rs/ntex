use std::cell::Cell;
use std::future::{pending, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use crate::{System, testing::TestRunner};

struct DropSignal {
    drops: Rc<Cell<usize>>,
    tx: Option<oneshot::Sender<()>>,
}

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
        let _ = self.tx.take().unwrap().send(());
    }
}

fn drop_signal(drops: &Rc<Cell<usize>>) -> (DropSignal, oneshot::AsyncReceiver<()>) {
    let (tx, rx) = oneshot::async_channel();
    (
        DropSignal {
            drops: drops.clone(),
            tx: Some(tx),
        },
        rx,
    )
}

/// STATEMENT: Native join handles are pointer-sized and support cancellation and completion.
///
/// HOW:
/// - Cancel a pending task, await a returned value, and observe another handle finish.
/// - Check runtime notification and join-error formatting.
#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
#[test]
fn join_handle() {
    assert_eq!(size_of::<crate::JoinHandle<usize>>(), size_of::<usize>());
    System::new("test", TestRunner).block_on(async {
        let hnd = crate::spawn(pending::<()>());
        assert!(!hnd.is_finished());
        hnd.cancel();

        assert_eq!(crate::spawn(async { 1 }).await.unwrap(), 1);

        let hnd = crate::spawn(async {});
        poll_fn(|cx| {
            if hnd.is_finished() {
                std::task::Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await;

        let hnd = crate::Handle::current().clone();
        hnd.notify().unwrap();
    });
    assert_eq!(super::JoinError.to_string(), "JoinError");
    assert_eq!(crate::rt::JoinError.to_string(), "JoinError");
}

/// STATEMENT: Cancellation-on-drop prevents an unpolled task from running and cleans up once.
///
/// HOW:
/// - Capture a drop signal in a task and opt in twice before yielding to the runtime.
/// - Drop or explicitly cancel the handle, then observe cleanup without any task execution.
#[test]
fn cancel_on_drop_before_first_poll() {
    System::new("cancel-on-drop", TestRunner).block_on(async {
        for explicit in [false, true] {
            let drops = Rc::new(Cell::new(0));
            let (guard, dropped) = drop_signal(&drops);
            let ran = Rc::new(Cell::new(false));
            let polled = ran.clone();
            let task = crate::spawn(async move {
                let _guard = guard;
                polled.set(true);
            })
            .cancel_on_drop()
            .cancel_on_drop();

            if explicit {
                task.cancel();
            } else {
                drop(task);
            }
            dropped.await.unwrap();
            assert!(!ran.get());
            assert_eq!(drops.get(), 1);
        }
    });
}

/// STATEMENT: Dropping a suspended opted-in handle cancels its task, including during unwinding.
///
/// HOW:
/// - Let an opted-in task reach a gated suspension and confirm it is unfinished.
/// - Drop the handle normally or during a caught panic, then open the gate.
/// - Confirm the continuation never runs and the future is destroyed exactly once.
#[test]
fn cancel_on_drop_pending_and_unwinding() {
    System::new("cancel-on-drop", TestRunner).block_on(async {
        for unwind in [false, true] {
            let drops = Rc::new(Cell::new(0));
            let (guard, dropped) = drop_signal(&drops);
            let (started, start) = oneshot::async_channel();
            let (release, gate) = oneshot::async_channel();
            let ran = Rc::new(Cell::new(false));
            let continued = ran.clone();
            let task = crate::spawn(async move {
                let _guard = guard;
                started.send(()).unwrap();
                gate.await.unwrap();
                continued.set(true);
            })
            .cancel_on_drop();
            start.await.unwrap();
            assert!(!task.is_finished());

            if unwind {
                assert!(
                    catch_unwind(AssertUnwindSafe(move || {
                        let _task = task;
                        panic!("drop opted-in handle");
                    }))
                    .is_err()
                );
            } else {
                drop(task);
            }
            let _ = release.send(());
            dropped.await.unwrap();
            assert!(!ran.get());
            assert_eq!(drops.get(), 1);
        }
    });
}

/// STATEMENT: Default handle drop and explicit detachment both let pending tasks continue.
///
/// HOW:
/// - Suspend a task, optionally enabling cancellation-on-drop before its first poll.
/// - Drop the default handle or explicitly detach the opted-in handle, then open the gate.
/// - Confirm both continuations run and their futures are destroyed once.
#[test]
#[allow(
    deprecated,
    reason = "verify legacy detach overrides cancellation-on-drop"
)]
fn cancel_on_drop_detach_override() {
    System::new("cancel-on-drop", TestRunner).block_on(async {
        for opt_in in [false, true] {
            let drops = Rc::new(Cell::new(0));
            let (guard, dropped) = drop_signal(&drops);
            let (started, start) = oneshot::async_channel();
            let (release, gate) = oneshot::async_channel();
            let ran = Rc::new(Cell::new(false));
            let continued = ran.clone();
            let task = crate::spawn(async move {
                let _guard = guard;
                started.send(()).unwrap();
                gate.await.unwrap();
                continued.set(true);
            });
            let task = if opt_in { task.cancel_on_drop() } else { task };
            start.await.unwrap();

            if opt_in {
                task.detach();
            } else {
                drop(task);
            }
            release.send(()).unwrap();
            dropped.await.unwrap();
            assert!(ran.get());
            assert_eq!(drops.get(), 1);
        }
    });
}

/// STATEMENT: Opted-in handles preserve completion and release non-Send results exactly once.
///
/// HOW:
/// - Complete a task returning a drop-tracked non-Send output and observe it finish.
/// - Await one result and drop another unread handle, enabling the option again after completion.
/// - Check outputs stay alive until consumed or discarded and are destroyed once.
#[test]
fn cancel_on_drop_completion() {
    System::new("cancel-on-drop", TestRunner).block_on(async {
        for await_result in [false, true] {
            let drops = Rc::new(Cell::new(0));
            let (output, dropped) = drop_signal(&drops);
            let task = crate::spawn(async move { output }).cancel_on_drop();
            poll_fn(|cx| {
                if task.is_finished() {
                    std::task::Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            assert_eq!(drops.get(), 0);

            let task = task.cancel_on_drop();
            if await_result {
                let output = task.await.unwrap();
                assert_eq!(drops.get(), 0);
                drop(output);
            } else {
                drop(task);
            }
            dropped.await.unwrap();
            assert_eq!(drops.get(), 1);
        }
    });
}

/// STATEMENT: An opted-in handle can outlive runtime shutdown without repeating future cleanup.
///
/// HOW:
/// - Return a pending task's opted-in handle, optionally waiting for its first poll.
/// - Shut down the runtime while retaining the handle.
/// - Check shutdown destroys the future once and marks the handle finished.
/// - Drop the surviving handle and confirm no additional destruction.
#[test]
fn cancel_on_drop_runtime_shutdown() {
    for poll_first in [false, true] {
        let (task, drops) = System::new("cancel-on-drop", TestRunner).block_on(async move {
            let drops = Rc::new(Cell::new(0));
            let (guard, _dropped) = drop_signal(&drops);
            let (started, start) = oneshot::async_channel();
            let task = crate::spawn(async move {
                let _guard = guard;
                let _ = started.send(());
                pending::<()>().await;
            })
            .cancel_on_drop();
            if poll_first {
                start.await.unwrap();
            }
            (task, drops)
        });
        assert_eq!(drops.get(), 1);
        assert!(task.is_finished());
        drop(task);
        assert_eq!(drops.get(), 1);
    }
}

/// STATEMENT: A sendable result is woken on another thread while its non-Send task stays local.
///
/// HOW:
/// - Spawn a gated task holding an Rc and poll its result on a plain thread.
/// - Release the task after the remote poll returns pending and require a wake notification.
/// - Receive a Send but non-Sync output and verify owner-thread execution and cleanup.
#[test]
fn spawn_sendable_cross_thread() {
    use std::sync::{Arc, mpsc};
    use std::task::{Context, Poll, Wake, Waker};
    use std::thread;
    use std::time::Duration;

    struct Notify(mpsc::Sender<()>);

    impl Wake for Notify {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }
    }

    System::new("spawn-sendable", TestRunner).block_on(async {
        let owner = thread::current().id();
        let drops = Rc::new(Cell::new(0));
        let (guard, dropped) = drop_signal(&drops);
        let (release, gate) = oneshot::async_channel();
        let result = crate::spawn_sendable(async move {
            let _guard = guard;
            assert_eq!(thread::current().id(), owner);
            gate.await.unwrap();
            assert_eq!(thread::current().id(), owner);
            Cell::new(42)
        });
        let (polled, remote_poll) = oneshot::async_channel();
        let (done, finished) = oneshot::async_channel();
        let reader = thread::spawn(move || {
            let (tx, notifications) = mpsc::channel();
            let waker = Waker::from(Arc::new(Notify(tx)));
            let mut cx = Context::from_waker(&waker);
            let mut result = std::pin::pin!(result);
            assert!(result.as_mut().poll(&mut cx).is_pending());
            polled.send(()).unwrap();
            notifications.recv_timeout(Duration::from_secs(5)).unwrap();
            let Poll::Ready(Ok(value)) = result.as_mut().poll(&mut cx) else {
                panic!("result not ready after completion notification");
            };
            assert_eq!(value.get(), 42);
            let mut dropped = std::pin::pin!(dropped);
            assert!(matches!(
                dropped.as_mut().poll(&mut cx),
                Poll::Ready(Ok(()))
            ));
            done.send(()).unwrap();
        });
        remote_poll.await.unwrap();
        release.send(()).unwrap();
        finished.await.unwrap();
        assert_eq!(drops.get(), 1);
        reader.join().unwrap();
    });
}

/// STATEMENT: Dropping a sendable result remotely cancels unpolled and suspended local tasks.
///
/// HOW:
/// - Capture a non-Send drop signal and optionally wait for the task's first poll.
/// - Drop the result on another thread, both before and after polling it, without releasing the task.
/// - Observe exactly one cleanup and no execution when cancelled before the first task poll.
#[test]
fn spawn_sendable_cancel_on_drop() {
    System::new("spawn-sendable", TestRunner).block_on(async {
        for (poll_first, poll_result) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let drops = Rc::new(Cell::new(0));
            let (guard, dropped) = drop_signal(&drops);
            let ran = Rc::new(Cell::new(false));
            let polled = ran.clone();
            let (started, start) = oneshot::async_channel();
            let result = crate::spawn_sendable(async move {
                let _guard = guard;
                polled.set(true);
                let _ = started.send(());
                pending::<()>().await;
            });
            if poll_first {
                start.await.unwrap();
            }
            std::thread::spawn(move || {
                let mut result = Box::pin(result);
                if poll_result {
                    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                    assert!(result.as_mut().poll(&mut cx).is_pending());
                }
                drop(result);
            })
            .join()
            .unwrap();
            dropped.await.unwrap();
            assert_eq!(ran.get(), poll_first);
            assert_eq!(drops.get(), 1);
        }
    });
}

/// STATEMENT: Runtime shutdown fails surviving sendable results and destroys local futures.
///
/// HOW:
/// - Retain the result of a pending task across shutdown, with or without waiting for a poll.
/// - Check that its non-Send state was cleaned up and poll the result on another thread.
/// - Require a ready join error rather than a stranded receiver.
#[test]
fn spawn_sendable_runtime_shutdown() {
    for poll_first in [false, true] {
        let (result, drops) = System::new("spawn-sendable", TestRunner).block_on(async move {
            let drops = Rc::new(Cell::new(0));
            let (guard, _dropped) = drop_signal(&drops);
            let (started, start) = oneshot::async_channel();
            let result = crate::spawn_sendable(async move {
                let _guard = guard;
                let _ = started.send(());
                pending::<()>().await;
            });
            if poll_first {
                start.await.unwrap();
            }
            (result, drops)
        });
        assert_eq!(drops.get(), 1);
        std::thread::spawn(move || {
            let mut result = std::pin::pin!(result);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(matches!(
                result.as_mut().poll(&mut cx),
                std::task::Poll::Ready(Err(_))
            ));
        })
        .join()
        .unwrap();
    }
}

/// STATEMENT: A task panic fails its sendable result rather than leaving the receiver pending.
///
/// HOW:
/// - Retain the result outside the runtime and wait for its task to start panicking.
/// - Allow runtimes that propagate task panics to unwind through a catch boundary.
/// - Poll the surviving result from another thread and require a ready join error.
#[test]
fn spawn_sendable_task_panic() {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = catch_unwind(AssertUnwindSafe(|| {
        System::new("spawn-sendable", TestRunner).block_on(async move {
            let (started, start) = oneshot::async_channel();
            let result = crate::spawn_sendable(async move {
                started.send(()).unwrap();
                panic!("sendable task panic");
            });
            tx.send(result).unwrap();
            start.await.unwrap();
        });
    }));
    let result = rx.try_recv().unwrap();
    std::thread::spawn(move || {
        let mut result = std::pin::pin!(result);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            result.as_mut().poll(&mut cx),
            std::task::Poll::Ready(Err(_))
        ));
    })
    .join()
    .unwrap();
}
