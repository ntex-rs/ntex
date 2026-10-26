use std::cell::RefCell;
use std::future::{pending, poll_fn};
use std::panic::{AssertUnwindSafe, Location, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{io, thread};

use super::*;
use crate::{Notify, PollResult, Runtime};

#[derive(Debug)]
struct NoopNotify;

impl Notify for NoopNotify {
    fn notify(&self) -> io::Result<()> {
        Ok(())
    }
}

fn runtime() -> Runtime {
    Runtime::new(Box::new(NoopNotify))
}

fn spawn<F: Future>(rt: &Runtime, future: F) -> Task<F::Output> {
    unsafe { super::spawn(future, &rt.queue, Location::caller()) }
}

fn drain(rt: &Runtime) {
    while rt.poll() == PollResult::PollAgain {}
}

#[test]
fn untouched_final_releases_do_not_need_fifo_service() {
    let rt = runtime();
    for kind in 0..3 {
        let mut task = spawn(&rt, poll_fn(|cx| Poll::Ready(cx.waker().clone())));
        drain(&rt);
        let Poll::Ready(Some(waker)) = task.poll_join(&Context::from_waker(Waker::noop())) else {
            panic!("missing completed waker");
        };
        let local = crate::LocalWaker::from_waker(&waker);
        match kind {
            0 => drop((waker, local, task)),
            1 => drop((task, local, waker)),
            _ => drop((task, waker, local)),
        }
        assert!(rt.queue.wake_queue.is_empty());
        assert_eq!(rt.queue.task_count(), 0);
    }
    rt.spawn_detached(async {});
    rt.queue.pop_local().unwrap().run();
    assert!(rt.queue.wake_queue.is_empty());
    assert_eq!(rt.queue.task_count(), 0);
}

#[test]
fn remote_touch_stays_ordered_after_fifo_drains() {
    let rt = runtime();
    let mut task = spawn(&rt, poll_fn(|cx| Poll::Ready(cx.waker().clone())));
    drain(&rt);
    let Poll::Ready(Some(waker)) = task.poll_join(&Context::from_waker(Waker::noop())) else {
        panic!("missing completed waker");
    };
    drop(task);
    let waker = thread::spawn(move || {
        drop(waker.clone());
        waker
    })
    .join()
    .unwrap();
    drain(&rt);
    assert!(rt.queue.wake_queue.is_empty());
    drop(waker);
    assert!(!rt.queue.wake_queue.is_empty());
    assert_eq!(rt.queue.task_count(), 1);
    drain(&rt);
    assert_eq!(rt.queue.task_count(), 0);
}

#[test]
fn owned_wakes_transfer_references_without_fifo_releases() {
    for local in [false, true] {
        let rt = runtime();
        let slot = Rc::new(RefCell::new(None));
        let saved = slot.clone();
        let task = spawn(
            &rt,
            poll_fn(move |cx| {
                *saved.borrow_mut() = Some(cx.waker().clone());
                Poll::<()>::Pending
            }),
        );
        drain(&rt);
        let waker = slot.borrow_mut().take().unwrap();
        // Leave a remote acquisition pending, owner releases must not overtake it.
        let remote = waker.clone();
        let (remote, clone) = thread::spawn(move || {
            let clone = remote.clone();
            (remote, clone)
        })
        .join()
        .unwrap();

        if local {
            crate::LocalWaker::from_waker(&waker).wake();
        } else {
            let owned = waker.clone();
            owned.wake();
        }
        let entries: Vec<_> = std::iter::from_fn(|| rt.queue.wake_queue.pop()).collect();
        assert_eq!(
            entries.len(),
            1,
            "a woken reference is released through the FIFO"
        );
        for entry in entries {
            assert!(entry.into_runnable(&rt.queue).is_none());
        }

        drop((waker, remote, clone));
        task.cancel();
        drain(&rt);
        assert!(slot.borrow().is_none());
        assert_eq!(rt.queue.task_count(), 0);
    }
}

#[test]
fn local_results_survive_runtime_shutdown() {
    for consume in [false, true] {
        let rt = runtime();
        let queue = rt.queue.clone();
        let drops = Rc::new(Cell::new(0));
        let result = DropCheck {
            owner: thread::current().id(),
            drops: drops.clone(),
            panics: false,
        };
        let mut task = spawn(&rt, async move { result });
        drain(&rt);
        drop(rt);
        assert!(task.is_finished());
        assert_eq!(drops.get(), 0);
        if consume {
            let Poll::Ready(Some(result)) = task.poll_join(&Context::from_waker(Waker::noop()))
            else {
                panic!("completed local result lost during shutdown");
            };
            drop(task);
            assert_eq!(drops.get(), 0);
            drop(result);
        } else {
            task.cancel();
        }
        assert_eq!(drops.get(), 1);
        assert_eq!(queue.task_count(), 0);
    }
}

#[test]
fn native_join_awaiters_release_after_unregister() {
    let rt = runtime();
    let queue = rt.queue.clone();
    let mut inner = spawn(&rt, pending::<()>());
    rt.spawn_detached(async move {
        let _ = poll_fn(|cx| inner.poll_join(cx)).await;
    });
    drain(&rt);
    assert_eq!(queue.task_count(), 2);
    drop(rt);
    assert_eq!(queue.task_count(), 0);
}

#[test]
fn raw_queue_survivors_release_after_shutdown() {
    for join_first in [false, true] {
        let rt = runtime();
        let mut task = spawn(&rt, poll_fn(|cx| Poll::Ready(cx.waker().clone())));
        drain(&rt);
        let Poll::Ready(Some(waker)) = task.poll_join(&Context::from_waker(Waker::noop())) else {
            panic!("missing completed waker");
        };
        let local = crate::LocalWaker::from_waker(&waker);
        thread::spawn(move || drop(waker)).join().unwrap();
        drain(&rt);
        assert_eq!(rt.queue.task_count(), 1);
        assert_eq!(0, Arc::weak_count(&rt.queue));
        let allocations = rt.queue.task_allocations.clone();
        drop(rt);

        let next = runtime();
        next.spawn_detached(poll_fn(|cx| {
            drop(cx.waker().clone());
            Poll::Ready(())
        }));
        drain(&next);
        if join_first {
            drop((task, local));
        } else {
            drop((local, task));
        }
        assert_eq!(Arc::strong_count(&allocations), 1);
        assert_eq!(next.queue.task_count(), 0);
    }
}

#[test]
fn raw_queue_shutdown_preserves_release_ordering() {
    struct ReleaseDuringDrain {
        task: Task<()>,
        local: Option<crate::LocalWaker>,
        deferred: Rc<Cell<bool>>,
        panics: bool,
    }

    impl Drop for ReleaseDuringDrain {
        fn drop(&mut self) {
            let refs = core(self.task.h).refs.get();
            drop(self.local.take());
            self.deferred.set(core(self.task.h).refs.get() == refs);
            assert!(!self.panics, "shutdown cleanup panic");
        }
    }

    for panics in [false, true] {
        let rt = runtime();
        let saved = Rc::new(RefCell::new(None));
        let slot = saved.clone();
        let task = spawn(
            &rt,
            poll_fn(move |cx| {
                *slot.borrow_mut() = Some(cx.waker().clone());
                Poll::Ready(())
            }),
        );
        drain(&rt);
        let waker = saved.borrow_mut().take().unwrap();
        let deferred = Rc::new(Cell::new(false));
        let release = ReleaseDuringDrain {
            task,
            local: Some(crate::LocalWaker::from_waker(&waker)),
            deferred: deferred.clone(),
            panics,
        };
        // Build the submission on its owner so its destructor can hold local handles.
        let h = alloc(
            Stage::Running(async move {
                let _release = release;
                pending::<()>().await;
            }),
            &rt.queue,
            Location::caller(),
            SCHEDULED | DETACHED,
            1,
        );
        rt.queue.wake_queue.push(WakeEntry::spawn(Remote(h)));
        thread::spawn(move || drop(waker)).join().unwrap();
        assert_eq!(rt.queue.task_count(), 2);
        let allocations = rt.queue.task_allocations.clone();
        let result = catch_unwind(AssertUnwindSafe(|| drop(rt)));
        assert_eq!(result.is_err(), panics);
        assert!(deferred.get(), "owner release overtook the pending FIFO");
        assert_eq!(Arc::strong_count(&allocations), 1);
    }
}

#[test]
fn raw_queue_release_retains_driver_during_callback() {
    use std::sync::atomic::AtomicBool;

    thread_local! {
        static OWNED_RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    }

    #[derive(Debug)]
    struct ShutdownNotify {
        armed: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl Notify for ShutdownNotify {
        fn notify(&self) -> io::Result<()> {
            if self.armed.swap(false, Ordering::Relaxed) {
                let rt = OWNED_RUNTIME.with(|slot| slot.borrow_mut().take());
                drop(rt);
                assert!(!self.dropped.load(Ordering::Relaxed));
            }
            Ok(())
        }
    }

    impl Drop for ShutdownNotify {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }

    let armed = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let rt = Runtime::new(Box::new(ShutdownNotify {
        armed: armed.clone(),
        dropped: dropped.clone(),
    }));
    let mut task = spawn(&rt, poll_fn(|cx| Poll::Ready(cx.waker().clone())));
    drain(&rt);
    let Poll::Ready(Some(waker)) = task.poll_join(&Context::from_waker(Waker::noop())) else {
        panic!("missing completed waker");
    };
    let local = crate::LocalWaker::from_waker(&waker);
    drop(task);
    thread::spawn(move || drop(waker)).join().unwrap();
    drain(&rt);
    let allocations = rt.queue.task_allocations.clone();
    OWNED_RUNTIME.with(|slot| *slot.borrow_mut() = Some(rt));
    armed.store(true, Ordering::Relaxed);
    drop(local);
    assert!(dropped.load(Ordering::Relaxed));
    assert_eq!(Arc::strong_count(&allocations), 1);
    OWNED_RUNTIME.with(|slot| assert!(slot.borrow().is_none()));
}

#[test]
fn runtime_ids_are_not_reused() {
    let first = runtime();
    let old = first.queue.runtime_id;
    drop(first);
    let second = runtime();
    assert_ne!(old, second.queue.runtime_id);
}

#[test]
fn owner_endpoint_cache_tracks_multiple_runtimes_and_shutdown() {
    let first = runtime();
    let second = runtime();
    let first_task = spawn(&first, async {});
    let second_task = spawn(&second, pending::<()>());
    drain(&first);
    drain(&second);
    let first_data = first_task.h.as_ptr().cast();
    let second_data = second_task.h.as_ptr().cast();
    for _ in 0..3 {
        assert!(unsafe { wake::active_owner(first_data) });
        assert!(unsafe { wake::active_owner(first_data) });
        assert!(unsafe { wake::active_owner(second_data) });
    }
    // Unregistering a different runtime must not invalidate the live identity.
    drop(first);
    assert!(unsafe { wake::active_owner(second_data) });
    assert!(!unsafe { wake::active_owner(first_data) });
    drop(second);
    assert!(!unsafe { wake::active_owner(second_data) });
    let third = runtime();
    assert!(!unsafe { wake::active_owner(first_data) });
    assert!(!unsafe { wake::active_owner(second_data) });
    drop((first_task, second_task, third));
}

#[test]
fn owner_endpoint_cache_does_not_cache_remote_routes() {
    let rt = runtime();
    let mut task = spawn(&rt, poll_fn(|cx| Poll::Ready(cx.waker().clone())));
    drain(&rt);
    let Poll::Ready(Some(waker)) = task.poll_join(&Context::from_waker(Waker::noop())) else {
        panic!("missing completed waker");
    };
    assert!(unsafe { wake::active_owner(waker.data()) });
    let waker = thread::spawn(move || {
        assert!(!unsafe { wake::active_owner(waker.data()) });
        drop(waker.clone());
        let other = runtime();
        let other_task = spawn(&other, pending::<()>());
        drain(&other);
        let other_data = other_task.h.as_ptr().cast();
        assert!(unsafe { wake::active_owner(other_data) });
        for _ in 0..2 {
            assert!(!unsafe { wake::active_owner(waker.data()) });
            drop(waker.clone());
            assert!(unsafe { wake::active_owner(other_data) });
        }
        drop(other_task);
        drain(&other);
        assert_eq!(other.queue.task_count(), 0);
        waker
    })
    .join()
    .unwrap();
    assert!(!rt.queue.wake_queue.is_empty());
    drain(&rt);
    assert!(unsafe { wake::active_owner(waker.data()) });
    drop((waker, task));
    drain(&rt);
    assert_eq!(rt.queue.task_count(), 0);
}

fn completed_waker(rt: &Runtime) -> Waker {
    let mut task = spawn(rt, poll_fn(|cx| Poll::Ready(cx.waker().clone())));
    drain(rt);
    let Poll::Ready(Some(waker)) = task.poll_join(&Context::from_waker(Waker::noop())) else {
        panic!("missing completed waker");
    };
    drop(task);
    waker
}

#[test]
fn remote_endpoint_cache_is_bounded_and_eviction_can_reenter() {
    use std::sync::{Mutex, mpsc};

    #[derive(Debug)]
    struct RetireNotify(Arc<Mutex<Option<Waker>>>);

    impl Notify for RetireNotify {
        fn notify(&self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for RetireNotify {
        fn drop(&mut self) {
            let waker = self.0.lock().unwrap().take();
            drop(waker);
        }
    }

    let release = Arc::new(Mutex::new(None));
    let first = Runtime::new(Box::new(RetireNotify(release.clone())));
    let second = runtime();
    let first_waker = completed_waker(&first);
    let second_waker = completed_waker(&second);
    *release.lock().unwrap() = Some(second_waker.clone());
    let first_queue = Arc::downgrade(&first.queue);
    let second_queue = Arc::downgrade(&second.queue);
    let (tx, incoming) = mpsc::channel::<Waker>();
    let (ack, rx) = mpsc::channel();
    let producer = thread::spawn(move || {
        for waker in incoming {
            drop(waker.clone());
            drop(waker);
            ack.send(()).unwrap();
        }
    });

    tx.send(first_waker).unwrap();
    rx.recv().unwrap();
    drain(&first);
    assert_eq!(first.queue.task_count(), 0);
    drop(first);
    assert!(first_queue.upgrade().is_some());

    tx.send(second_waker).unwrap();
    rx.recv().unwrap();
    assert!(
        first_queue.upgrade().is_none(),
        "previous endpoint retained"
    );
    assert!(release.lock().unwrap().is_none());
    drain(&second);
    assert_eq!(second.queue.task_count(), 0);
    drop(second);
    assert!(second_queue.upgrade().is_some());

    drop(tx);
    producer.join().unwrap();
    assert!(second_queue.upgrade().is_none(), "thread cache retained");
}

#[test]
fn remote_endpoint_cache_notification_can_reenter() {
    use std::sync::Mutex;

    #[derive(Debug)]
    struct ReenterNotify(Arc<Mutex<Option<Waker>>>);

    impl Notify for ReenterNotify {
        fn notify(&self) -> io::Result<()> {
            let waker = self.0.lock().unwrap().take();
            if let Some(waker) = waker {
                waker.wake();
            }
            Ok(())
        }
    }

    let target = Arc::new(Mutex::new(None));
    let first = Runtime::new(Box::new(ReenterNotify(target.clone())));
    let second = runtime();
    let waker = completed_waker(&first);
    *target.lock().unwrap() = Some(completed_waker(&second));
    thread::spawn(move || {
        drop(waker.clone());
        drop(waker);
    })
    .join()
    .unwrap();
    assert!(target.lock().unwrap().is_none());
    drain(&first);
    drain(&second);
    assert_eq!(first.queue.task_count(), 0);
    assert_eq!(second.queue.task_count(), 0);
}

#[test]
fn remote_endpoint_cache_falls_back_during_tls_destruction() {
    struct WakeOnExit(RefCell<Option<Waker>>);

    impl Drop for WakeOnExit {
        fn drop(&mut self) {
            self.0.get_mut().take().unwrap().wake();
        }
    }

    thread_local! {
        static ON_EXIT: WakeOnExit = const { WakeOnExit(RefCell::new(None)) };
    }

    let rt = runtime();
    let saved = Rc::new(RefCell::new(None));
    let slot = saved.clone();
    let polls = Rc::new(Cell::new(0));
    let p = polls.clone();
    let task = spawn(
        &rt,
        poll_fn(move |cx| {
            p.set(p.get() + 1);
            if p.get() == 1 {
                *slot.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }),
    );
    drain(&rt);
    let waker = saved.borrow_mut().take().unwrap();
    thread::spawn(move || {
        // Initialize this destructor first so the endpoint cache is destroyed first.
        ON_EXIT.with(|slot| *slot.0.borrow_mut() = Some(waker));
        ON_EXIT.with(|slot| drop(slot.0.borrow().as_ref().unwrap().clone()));
    })
    .join()
    .unwrap();
    drain(&rt);
    assert_eq!(polls.get(), 2);
    drop(task);
    drain(&rt);
    assert_eq!(rt.queue.task_count(), 0);
}

#[test]
fn borrowed_poll_waker_acquisition_precedes_final_owner_release() {
    for ready in [false, true] {
        for convert in [false, true] {
            let rt = runtime();
            let drops = Rc::new(Cell::new(0));
            let guard = DropCheck {
                owner: thread::current().id(),
                drops: drops.clone(),
                panics: false,
            };
            let saved = Rc::new(RefCell::new(None));
            let slot = saved.clone();
            rt.spawn_detached(async move {
                let _guard = guard;
                poll_fn(|cx| {
                    // Only the borrowed poll reference protects this acquisition.
                    let borrowed = cx.waker();
                    *slot.borrow_mut() = Some(thread::scope(|scope| {
                        scope.spawn(|| borrowed.clone()).join().unwrap()
                    }));
                    if ready { Poll::Ready(()) } else { Poll::Pending }
                })
                .await;
            });
            rt.queue.pop_local().unwrap().run();
            let waker = saved.borrow_mut().take().unwrap();
            assert_eq!(core(unsafe { task_ptr(waker.data()) }).refs.get(), 1);
            assert_eq!(rt.queue.task_count(), 1);
            // Owner release must not overtake the queued acquisition.
            if convert {
                drop(crate::LocalWaker::from(waker).into_waker());
            } else {
                drop(waker);
            }
            drain(&rt);
            assert_eq!(drops.get(), 1);
            assert_eq!(rt.queue.task_count(), 0);
        }
    }
}

#[test]
fn returned_remote_clone_keeps_its_source_alive_until_acquisition() {
    let rt = runtime();
    let saved = Rc::new(RefCell::new(None));
    let slot = saved.clone();
    rt.spawn_detached(poll_fn(move |cx| {
        *slot.borrow_mut() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    drain(&rt);
    let source = saved.borrow_mut().take().unwrap();
    let (source, cloned) = thread::spawn(move || {
        let cloned = source.clone();
        (source, cloned)
    })
    .join()
    .unwrap();
    drop((source, cloned));
    drain(&rt);
    assert!(rt.queue.tasks.head.get().is_null());
    assert_eq!(rt.queue.task_count(), 0);
}

#[test]
fn owner_release_does_not_overtake_queued_remote_releases() {
    for ready in [false, true] {
        for clone in [false, true] {
            let rt = runtime();
            let saved = Rc::new(RefCell::new(None));
            let slot = saved.clone();
            let polls = Rc::new(Cell::new(0));
            let count = polls.clone();
            let drops = Rc::new(Cell::new(0));
            let guard = DropCheck {
                owner: thread::current().id(),
                drops: drops.clone(),
                panics: false,
            };
            let task = spawn(&rt, async move {
                let _guard = guard;
                poll_fn(move |cx| {
                    count.set(count.get() + 1);
                    if count.get() == 1 {
                        *slot.borrow_mut() = Some(cx.waker().clone());
                        if !ready {
                            return Poll::Pending;
                        }
                    }
                    Poll::Ready(())
                })
                .await;
            });
            drain(&rt);
            let source = saved.borrow_mut().take().unwrap();
            let first = source.clone();
            let second = source.clone();
            drop(task);
            drain(&rt);
            let (source, cloned) = thread::spawn(move || {
                drop((first, second));
                let cloned = if clone {
                    Some(source.clone())
                } else {
                    source.wake_by_ref();
                    None
                };
                (source, cloned)
            })
            .join()
            .unwrap();
            drop(source);
            drain(&rt);
            if clone {
                assert_eq!(drops.get(), usize::from(ready));
            } else {
                assert_eq!(polls.get(), if ready { 1 } else { 2 });
            }
            drop(cloned);
            drain(&rt);
            assert_eq!(drops.get(), 1);
            assert_eq!(rt.queue.task_count(), 0);
        }
    }
}

#[test]
fn running_release_does_not_overtake_borrowed_acquisition() {
    let rt = runtime();
    let saved = Rc::new(RefCell::new(None));
    let slot = saved.clone();
    let drops = Rc::new(Cell::new(0));
    let guard = DropCheck {
        owner: thread::current().id(),
        drops: drops.clone(),
        panics: false,
    };
    rt.spawn_detached(async move {
        let _guard = guard;
        poll_fn(move |cx| {
            let borrowed = cx.waker();
            *slot.borrow_mut() = Some(thread::scope(|scope| {
                scope.spawn(|| borrowed.clone()).join().unwrap()
            }));
            Poll::<()>::Pending
        })
        .await;
    });
    rt.queue.pop_local().unwrap().run();
    saved.borrow_mut().take().unwrap().wake();
    rt.queue.pop_local().unwrap().run();
    let waker = saved.borrow_mut().take().unwrap();
    drain(&rt);
    assert_eq!(drops.get(), 0);
    drop(waker);
    drain(&rt);
    assert_eq!(drops.get(), 1);
    assert_eq!(rt.queue.task_count(), 0);
}

#[test]
fn earlier_fifo_wake_acquires_later_published_writes() {
    struct Published(std::cell::UnsafeCell<usize>);
    unsafe impl Sync for Published {}

    let rt = runtime();
    let saved = Rc::new(RefCell::new(None));
    let slot = saved.clone();
    let value = Arc::new(Published(std::cell::UnsafeCell::new(0)));
    let data = value.clone();
    let done = Arc::new(AtomicUsize::new(0));
    let published = done.clone();
    let mut first = true;
    let task = spawn(
        &rt,
        poll_fn(move |cx| {
            if first {
                first = false;
                *slot.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            } else {
                assert_eq!(unsafe { *data.0.get() }, 42);
                Poll::Ready(())
            }
        }),
    );
    drain(&rt);
    let waker = saved.borrow_mut().take().unwrap();
    let (waker, references) = thread::spawn(move || {
        let mut references = vec![waker.clone()];
        waker.wake_by_ref();
        references.extend((0..64).map(|_| waker.clone()));
        (waker, references)
    })
    .join()
    .unwrap();
    // Start consuming a backlog spanning segments before the later publication.
    assert!(
        rt.queue
            .wake_queue
            .pop()
            .unwrap()
            .into_runnable(&rt.queue)
            .is_none()
    );
    let producer = thread::spawn(move || {
        unsafe { *value.0.get() = 42 };
        waker.wake_by_ref();
        // Only wake publication, not this flag, synchronizes the payload.
        published.store(1, Ordering::Relaxed);
        waker
    });
    while done.load(Ordering::Relaxed) == 0 {
        thread::yield_now();
    }
    // Bypass Runtime::poll's initial notification acquisition.
    rt.queue
        .wake_queue
        .pop()
        .unwrap()
        .into_runnable(&rt.queue)
        .unwrap()
        .run();
    assert!(task.is_finished());
    drop(producer.join().unwrap());
    drop(references);
    drop(task);
    drain(&rt);
    assert_eq!(rt.queue.task_count(), 0);
}

#[test]
fn unregistered_clones_and_native_awaiters_are_inert() {
    let rt = runtime();
    let saved = Rc::new(RefCell::new(None));
    let slot = saved.clone();
    rt.spawn_detached(poll_fn(move |cx| {
        *slot.borrow_mut() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    drain(&rt);
    let waker = saved.borrow_mut().take().unwrap();
    let queue = rt.queue.clone();
    drop(rt);
    assert!(queue.is_closed());
    assert!(queue.tasks.head.get().is_null());
    assert_eq!(queue.task_count(), 1);
    let awaiter = Awaiter::new();
    assert!(unsafe { awaiter.register(&waker) }.is_none());
    let cloned = unsafe { awaiter.take() }.unwrap();
    assert!(!cloned.will_wake(&waker));
    cloned.wake();
    thread::spawn(move || {
        let cloned = waker.clone();
        assert!(!ptr::eq(cloned.vtable(), waker.vtable()));
        cloned.wake();
        waker.wake_by_ref();
        waker.wake();
    })
    .join()
    .unwrap();
    assert!(queue.wake_queue.is_empty());
    // Missing endpoints discard releases too: retaining storage is intentional.
    assert_eq!(queue.task_count(), 1);
    let weak = Arc::downgrade(&queue);
    drop(queue);
    assert!(weak.upgrade().is_none());
}

struct PinnedFuture {
    rt: Rc<Runtime>,
    address: Cell<*const Self>,
    owner: thread::ThreadId,
    drops: Rc<Cell<usize>>,
    waker: Rc<RefCell<Option<crate::LocalWaker>>>,
    _pin: PhantomPinned,
}

impl Future for PinnedFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.as_ref().get_ref();
        assert_eq!(thread::current().id(), this.owner);
        let address = ptr::from_ref(this);
        if this.address.get().is_null() {
            this.address.set(address);
            *this.waker.borrow_mut() = Some(crate::LocalWaker::from_waker(cx.waker()));
            // Reenter spawning and grow the unfinished-task list.
            for _ in 0..128 {
                this.rt.spawn_detached(async {});
            }
            Poll::Pending
        } else {
            assert_eq!(this.address.get(), address);
            Poll::Ready(())
        }
    }
}

impl Drop for PinnedFuture {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.owner);
        assert_eq!(self.address.get(), ptr::from_ref(self));
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn pinned_core_survives_reentrant_spawning() {
    let rt = Rc::new(runtime());
    let drops = Rc::new(Cell::new(0));
    let waker = Rc::new(RefCell::new(None));
    let mut task = spawn(
        &rt,
        PinnedFuture {
            rt: rt.clone(),
            address: Cell::new(ptr::null()),
            owner: thread::current().id(),
            drops: drops.clone(),
            waker: waker.clone(),
            _pin: PhantomPinned,
        },
    );
    let address = task.h;
    assert!(!rt.queue.pop_local().unwrap().run());
    assert_eq!(task.h, address);
    assert_ne!(core(task.h).state.get() & LISTED, 0);
    waker.borrow().as_ref().unwrap().wake_by_ref();
    drain(&rt);
    assert_eq!(drops.get(), 1);
    assert_eq!(core(task.h).state.get() & LISTED, 0);
    assert!(matches!(
        task.poll_join(&Context::from_waker(Waker::noop())),
        Poll::Ready(Some(()))
    ));
    drop(task);
    let local = waker.borrow_mut().take().unwrap();
    local.wake_by_ref();
    drop(local.clone());
    assert!(rt.queue.local_queue.is_empty());
    drop(local);
    drain(&rt);
    assert_eq!(rt.queue.task_count(), 0);
}

struct DropCheck {
    owner: thread::ThreadId,
    drops: Rc<Cell<usize>>,
    panics: bool,
}

impl Drop for DropCheck {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.owner);
        self.drops.set(self.drops.get() + 1);
        assert!(!self.panics, "future drop panic");
    }
}

struct CheckCompletion {
    queue: Arc<RunnableQueue>,
    calls: AtomicUsize,
}

impl std::task::Wake for CheckCompletion {
    fn wake(self: Arc<Self>) {
        assert_eq!(Some(self.queue.id), try_current_thread_id());
        assert!(self.queue.tasks.head.get().is_null());
        self.calls.fetch_add(1, Ordering::Relaxed);
        // Completion callbacks may immediately spawn another task.
        unsafe {
            super::spawn_detached::<_, true>(async {}, &self.queue, Location::caller());
        }
    }
}

#[test]
fn unlink_on_completion_cancel_panic_and_shutdown() {
    for path in 0..6 {
        let rt = runtime();
        let drops = Rc::new(Cell::new(0));
        let guard = DropCheck {
            owner: thread::current().id(),
            drops: drops.clone(),
            panics: path == 3,
        };
        let future = async move {
            let _guard = guard;
            assert_ne!(path, 2, "poll panic");
            if path != 0 {
                pending::<()>().await;
            }
        };
        let mut task = Some(spawn(&rt, future));
        let completion = Arc::new(CheckCompletion {
            queue: rt.queue.clone(),
            calls: AtomicUsize::new(0),
        });
        let waker = Waker::from(completion.clone());
        assert!(
            task.as_mut()
                .unwrap()
                .poll_join(&Context::from_waker(&waker))
                .is_pending()
        );
        let result = catch_unwind(AssertUnwindSafe(|| {
            match path {
                1 | 3 => task.take().unwrap().cancel(),
                5 => {
                    rt.queue.pop_local().unwrap().run();
                }
                _ => {}
            }
            if path >= 4 {
                rt.queue.shutdown();
            } else {
                rt.queue.pop_local().unwrap().run();
            }
        }));
        assert_eq!(result.is_err(), matches!(path, 2 | 3));
        assert_eq!(drops.get(), 1);
        if let Some(task) = &task {
            assert_eq!(core(task.h).state.get() & LISTED, 0);
        }
        assert_eq!(
            completion.calls.load(Ordering::Relaxed),
            usize::from(!matches!(path, 1 | 3))
        );
        if let Some(task) = &task {
            assert!(task.is_finished());
            assert_eq!(core(task.h).state.get() & LISTED, 0);
        }
        drop((task, waker, completion));
        drain(&rt);
        assert_eq!(rt.queue.task_count(), 0);
    }
}

#[test]
fn unfinished_list_does_not_retain_orphaned_futures() {
    for path in 0..3 {
        let rt = runtime();
        let drops = Rc::new(Cell::new(0));
        let guard = DropCheck {
            owner: thread::current().id(),
            drops: drops.clone(),
            panics: false,
        };
        let captured = Rc::new(RefCell::new(None));
        let slot = captured.clone();
        let task = spawn(&rt, async move {
            let _guard = guard;
            poll_fn(|cx| {
                *slot.borrow_mut() = Some(cx.waker().clone());
                Poll::<()>::Pending
            })
            .await;
        });
        assert!(!rt.queue.pop_local().unwrap().run());
        drop(task);
        let waker = captured.borrow_mut().take().unwrap();
        match path {
            0 => drop(waker),
            1 => {
                let local = crate::LocalWaker::from_waker(&waker);
                drop(waker);
                drop(local);
            }
            _ => thread::spawn(move || drop(waker)).join().unwrap(),
        }
        drain(&rt);
        assert_eq!(drops.get(), 1);
        assert!(rt.queue.tasks.head.get().is_null());
        assert_eq!(rt.queue.task_count(), 0);
    }
}

#[test]
fn remote_activation_and_shutdown_before_poll() {
    let rt = runtime();
    let queue = rt.queue.clone();
    let polls = Arc::new(AtomicUsize::new(0));
    let p = polls.clone();
    thread::spawn(move || unsafe {
        super::spawn_detached::<_, false>(
            poll_fn(move |_| {
                p.fetch_add(1, Ordering::Relaxed);
                Poll::<()>::Pending
            }),
            &queue,
            Location::caller(),
        );
    })
    .join()
    .unwrap();
    let remote = rt.queue.wake_queue.pop().unwrap().into_remote();
    assert_eq!(core(remote.0).state.get() & LISTED, 0);
    let runnable = remote.into_runnable();
    assert_ne!(core(runnable.0).state.get() & LISTED, 0);
    drop(runnable);
    assert_eq!(polls.load(Ordering::Relaxed), 0);
    drain(&rt);
    assert!(rt.queue.tasks.head.get().is_null());
    assert_eq!(rt.queue.task_count(), 0);
}

#[test]
fn shutdown_does_not_reactivate_a_running_core() {
    for remote in [false, true] {
        let rt = runtime();
        let queue = rt.queue.clone();
        let q = queue.clone();
        let mut owned_rt = Some(rt);
        let drops = Rc::new(Cell::new(0));
        let guard = DropCheck {
            owner: thread::current().id(),
            drops: drops.clone(),
            panics: false,
        };
        let task = unsafe {
            super::spawn(
                async move {
                    let _guard = guard;
                    poll_fn(move |cx| {
                        let h = task_ptr(local_waker_ptr(cx.waker()).unwrap());
                        if remote {
                            assert!(q.tasks.close_first());
                            let waker = cx.waker().clone();
                            thread::spawn(move || waker.wake()).join().unwrap();
                            assert!(q.wake_queue.pop().unwrap().into_runnable(&q).is_none());
                        } else {
                            drop(owned_rt.take());
                            cx.waker().wake_by_ref();
                        }

                        assert_eq!(core(h).state.get() & LISTED, 0);
                        Poll::<()>::Pending
                    })
                    .await;
                },
                &queue,
                Location::caller(),
            )
        };
        assert!(!queue.pop_local().unwrap().run());
        assert_eq!(drops.get(), 1);
        assert!(task.is_finished());
        assert_eq!(core(task.h).state.get() & LISTED, 0);
        while let Some(entry) = queue.wake_queue.pop() {
            assert!(entry.into_runnable(&queue).is_none());
        }
        assert!(queue.local_queue.is_empty());
        assert!(queue.wake_queue.is_empty());
        drop(task);
        assert_eq!(queue.task_count(), 0);
    }
}

#[test]
fn completed_wakers_retain_storage_but_not_the_driver() {
    let rt = runtime();
    let saved = Rc::new(RefCell::new(None));
    let slot = saved.clone();
    let drops = Rc::new(Cell::new(0));
    let guard = DropCheck {
        owner: thread::current().id(),
        drops: drops.clone(),
        panics: false,
    };
    let task = spawn(&rt, async move {
        let _guard = guard;
        let payload = [42u8; 4096];
        poll_fn(|cx| {
            *slot.borrow_mut() = Some(cx.waker().clone());
            std::hint::black_box(&payload);
            Poll::Ready(())
        })
        .await;
    });
    drain(&rt);
    drop(task);
    assert_eq!(drops.get(), 1);
    assert_eq!(rt.queue.task_count(), 1);

    let polls = Rc::new(Cell::new(0));
    let p = polls.clone();
    let next = spawn(
        &rt,
        poll_fn(move |_| {
            p.set(p.get() + 1);
            Poll::<()>::Pending
        }),
    );
    drain(&rt);
    let late = saved.borrow_mut().take().unwrap();
    late.wake_by_ref();
    let late = thread::spawn(move || {
        Waker::wake(late.clone());
        late
    })
    .join()
    .unwrap();
    drain(&rt);
    assert_eq!(polls.get(), 1);
    next.cancel();
    drain(&rt);
    let queue = Arc::downgrade(&rt.queue);
    drop(rt);
    assert!(queue.upgrade().is_none());
    thread::spawn(move || late.wake()).join().unwrap();
}

#[test]
fn segqueue_publishes_all_producers_writes() {
    let rt = runtime();
    let threads = if cfg!(miri) { 2 } else { 8 };
    let rounds = if cfg!(miri) { 2 } else { 128 };
    let writes = Arc::new(
        (0..threads)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let done = Arc::new(AtomicUsize::new(0));
    let saved = Rc::new(RefCell::new(None));
    let (slot, values) = (saved.clone(), writes.clone());
    let mut polled = false;
    let task = spawn(
        &rt,
        poll_fn(move |cx| {
            if polled {
                for value in values.iter() {
                    assert_eq!(value.load(Ordering::Relaxed), rounds);
                }
                Poll::Ready(())
            } else {
                polled = true;
                *slot.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        }),
    );
    drain(&rt);
    let waker = saved.borrow_mut().take().unwrap();
    let producers: Vec<_> = (0..threads)
        .map(|index| {
            let (waker, writes, done) = (waker.clone(), writes.clone(), done.clone());
            thread::spawn(move || {
                for value in 1..=rounds {
                    writes[index].store(value, Ordering::Relaxed);
                    waker.wake_by_ref();
                    waker.clone().wake();
                    drop(waker.clone());
                }
                done.fetch_add(1, Ordering::Relaxed);
            })
        })
        .collect();
    while done.load(Ordering::Relaxed) != threads {
        thread::yield_now();
    }
    drain(&rt);
    assert!(task.is_finished());
    for producer in producers {
        producer.join().unwrap();
    }
    drop((task, waker));
    drain(&rt);
    assert_eq!(rt.queue.task_count(), 0);
}

#[test]
fn last_external_wake_runs_but_drop_only_cancels() {
    for wake in [false, true] {
        let rt = runtime();
        let drops = Rc::new(Cell::new(0));
        let guard = DropCheck {
            owner: thread::current().id(),
            drops: drops.clone(),
            panics: false,
        };
        let polls = Rc::new(Cell::new(0));
        let saved = Rc::new(RefCell::new(None));
        let (slot, p) = (saved.clone(), polls.clone());
        rt.spawn_detached(async move {
            let _guard = guard;
            poll_fn(|cx| {
                p.set(p.get() + 1);
                if p.get() == 1 {
                    *slot.borrow_mut() = Some(cx.waker().clone());
                }
                Poll::<()>::Pending
            })
            .await;
        });
        drain(&rt);
        let waker = saved.borrow_mut().take().unwrap();
        thread::spawn(move || {
            if wake {
                waker.wake();
            } else {
                drop(waker);
            }
        })
        .join()
        .unwrap();
        assert_eq!(drops.get(), 0);
        drain(&rt);
        assert_eq!(polls.get(), if wake { 2 } else { 1 });
        assert_eq!(drops.get(), 1);
        assert_eq!(rt.queue.task_count(), 0);
    }
}

#[test]
fn shutdown_drains_admitted_wake_entries() {
    let rt = runtime();
    let drops = Rc::new(Cell::new(0));
    let guard = DropCheck {
        owner: thread::current().id(),
        drops: drops.clone(),
        panics: false,
    };
    let saved = Rc::new(RefCell::new(None));
    let slot = saved.clone();
    rt.spawn_detached(async move {
        let _guard = guard;
        poll_fn(|cx| {
            *slot.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        })
        .await;
    });
    drain(&rt);
    let waker = saved.borrow_mut().take().unwrap();
    thread::spawn(move || waker.wake()).join().unwrap();
    let entry = rt.queue.wake_queue.pop().unwrap();
    let queue = rt.queue.clone();
    let weak = Arc::downgrade(&queue);
    let (tx, rx) = std::sync::mpsc::channel();
    let producer = thread::spawn(move || {
        let permit = queue.admit(false).unwrap();
        tx.send(()).unwrap();
        while !queue.is_closed() {
            thread::yield_now();
        }
        if let Some(queue) = permit.publish(entry) {
            queue.notify();
        }
    });
    rx.recv().unwrap();
    drop(rt);
    producer.join().unwrap();
    assert_eq!(drops.get(), 1);
    assert!(weak.upgrade().is_none());
}
