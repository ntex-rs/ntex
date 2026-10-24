#![allow(clippy::cast_possible_truncation)]
use std::collections::{BTreeMap, VecDeque};
use std::task::Poll;
use std::time::{Duration, Instant};
use std::{cell::Cell, cell::RefCell, mem, num::NonZeroUsize, ops, rc::Rc};

use ntex_rt::{Arbiter, LocalWaker};
use ntex_util::time::{Millis, Seconds, now, sleep};
use ntex_util::{HashSet, spawn_detached};
use slab::Slab;

use crate::IoRef;

const CAP: usize = 32;
/// Timer sets with a larger capacity are dropped instead of cached.
const MAX_CACHED_SET_CAP: usize = 512;

thread_local! {
    static MANAGER: RefCell<Option<IoManager>> = const { RefCell::new(None) };
    /// Id of the last spawned write task, unique across managers of the thread.
    static IOPS_TASK: Cell<u64> = const { Cell::new(0) };
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
/// Opaque identifier assigned to a registered I/O stream.
///
/// The identifier is meaningful only within the current thread's I/O manager.
pub struct Id(Option<NonZeroUsize>);

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
/// Handle to an I/O dispatcher timer.
///
/// Handles represent second-granularity deadlines managed by the current
/// thread's I/O manager, a timer started with a `t` seconds timeout expires
/// after at least `t` and less than `t + 1` seconds. The timer clock starts
/// a new second when a timer starts while no other timers are pending, such
/// a timer expires after `t` seconds. A handle becomes stale
/// after its timer is stopped or replaced and must not be used as an
/// independent cancellation token.
pub struct TimerHandle(u32);

impl TimerHandle {
    /// A handle that does not refer to an active timer.
    pub const ZERO: TimerHandle = TimerHandle(0);

    /// Returns `true` if this handle refers to a timer deadline.
    ///
    /// This does not guarantee that the timer is still registered or has not
    /// already elapsed.
    pub fn is_set(&self) -> bool {
        self.0 != 0
    }

    /// Returns the whole seconds remaining until this handle's deadline.
    ///
    /// Returns zero if the deadline has elapsed. The remaining time is
    /// measured from the cached [`now()`](ntex_util::time::now), which lags
    /// the clock, and is rounded up.
    pub fn remains(&self) -> Seconds {
        let rem = self.instant().saturating_duration_since(now());
        let secs = rem.as_secs() + u64::from(rem.subsec_nanos() != 0);
        Seconds(secs.min(u64::from(u16::MAX)) as u16)
    }

    /// Returns the instant represented by this handle.
    ///
    /// The instant is based on the current thread's I/O timer clock. For
    /// [`ZERO`](Self::ZERO), this is the clock's base instant.
    pub fn instant(&self) -> Instant {
        IoManager::with(|mgr| mgr.timers.base + Duration::from_secs(u64::from(self.0)))
    }

    pub(crate) fn update(self, timeout: Seconds, io: &IoRef) -> TimerHandle {
        IoManager::with(|mgr| {
            let new_hnd = mgr.timers.deadline(timeout);
            if self.0 == new_hnd || self.0 == new_hnd + 1 {
                self
            } else {
                mgr.timers.unregister(self, io);
                mgr.timers.register(timeout, io)
            }
        })
    }

    pub(crate) fn unregister(self, io: &IoRef) {
        IoManager::with(|manager| manager.timers.unregister(self, io));
    }

    pub(crate) fn register(timeout: Seconds, io: &IoRef) -> TimerHandle {
        IoManager::with(move |mgr| mgr.timers.register(timeout, io))
    }
}

impl ops::Add<Seconds> for TimerHandle {
    type Output = TimerHandle;

    #[inline]
    fn add(self, other: Seconds) -> TimerHandle {
        TimerHandle(self.0 + u32::from(other.0))
    }
}

struct TimerStorage {
    running: bool,
    base: Instant,
    /// Whole seconds elapsed since `base`, the keys up to it have expired.
    current: u32,
    cache: VecDeque<HashSet<Id>>,
    notifications: BTreeMap<u32, HashSet<Id>>,
}

impl TimerStorage {
    fn unregister(&mut self, hnd: TimerHandle, io: &IoRef) {
        if let Some(items) = self.notifications.get_mut(&hnd.0) {
            items.remove(&io.id());
            if items.is_empty() {
                // the timer stops once no timers are left
                let items = self.notifications.remove(&hnd.0).unwrap();
                self.recycle(items);
            }
        }
    }

    /// Returns an empty timer set to the cache, a set grown by a burst of
    /// timers is dropped so its capacity is not retained.
    fn recycle(&mut self, items: HashSet<Id>) {
        debug_assert!(items.is_empty());
        if self.cache.len() < CAP && items.capacity() <= MAX_CACHED_SET_CAP {
            self.cache.push_back(items);
        }
    }

    /// Updates `current` from the clock.
    fn update_current(&mut self) -> u32 {
        self.current = self.base.elapsed().as_secs() as u32;
        self.current
    }

    /// Returns the key of a timer started now, it expires after at least
    /// `timeout` and less than `timeout + 1` seconds.
    ///
    /// The start is the cached [`now()`](ntex_util::time::now), a timer
    /// expires early by the age of the cached time.
    fn deadline(&mut self, timeout: Seconds) -> u32 {
        let elapsed = now().saturating_duration_since(self.base);
        let secs = elapsed.as_secs() as u32;
        if secs < self.current {
            // the cached time lags the ticker, keys up to `current` expired
            return self.current + 1 + u32::from(timeout.0);
        }
        self.current = secs;

        if self.notifications.is_empty() {
            // no timers are pending, start the clock second now so a timer
            // expires after exactly `timeout`. `current` does not change,
            // stale handles cannot match a new key
            self.base += Duration::new(0, elapsed.subsec_nanos());
            self.current + u32::from(timeout.0)
        } else {
            let partial = u32::from(elapsed.subsec_nanos() != 0);
            self.current + partial + u32::from(timeout.0)
        }
    }

    fn register(&mut self, timeout: Seconds, io: &IoRef) -> TimerHandle {
        let hnd = self.deadline(timeout);
        if let Some(items) = self.notifications.get_mut(&hnd) {
            items.insert(io.id());
        } else {
            let mut items = self.cache.pop_front().unwrap_or_default();
            items.insert(io.id());
            self.notifications.insert(hnd, items);
        }

        self.run_timer();

        TimerHandle(hnd)
    }

    fn run_timer(&mut self) {
        if self.running {
            return;
        }
        self.running = true;

        spawn_detached(async move {
            let guard = TimerGuard;
            // one wheel timer is reset for every tick
            let timer = sleep(Self::next_tick());
            loop {
                timer.wait().await;

                let stop = IoManager::with(|mgr| {
                    let current = mgr.timers.update_current();

                    // notify io dispatchers of all expired timers
                    while let Some(entry) = mgr.timers.notifications.first_entry() {
                        if *entry.key() > current {
                            break;
                        }
                        let mut items = entry.remove();
                        for id in items.drain() {
                            if let Some(io) = mgr.get(id) {
                                io.notify_timeout();
                            }
                        }
                        mgr.timers.recycle(items);
                    }

                    if mgr.timers.notifications.is_empty() {
                        mgr.timers.running = false;
                        true
                    } else {
                        false
                    }
                });

                if stop {
                    break;
                }
                timer.reset(Self::next_tick());
            }
            drop(guard);
        });
    }

    /// Returns the delay until the next whole second of the timer clock, a
    /// late tick does not delay the later ones.
    fn next_tick() -> Millis {
        let next = IoManager::with(|mgr| {
            let t = &mgr.timers;
            let next = t.base + Duration::from_secs(u64::from(t.current) + 1);
            next.saturating_duration_since(Instant::now())
        });
        Millis(next.as_millis() as u32 + 1)
    }
}

struct TimerGuard;

impl Drop for TimerGuard {
    fn drop(&mut self) {
        IoManager::with(|mgr| {
            mgr.timers.running = false;
            mgr.timers.notifications.clear();
        });
    }
}

pub(crate) struct IoManager {
    storage: Slab<Option<IoRef>>,
    timers: TimerStorage,
    pub(crate) iops: Iops,
}

impl Default for IoManager {
    fn default() -> IoManager {
        let mut storage = Slab::new();
        assert_eq!(storage.insert(None), 0);

        IoManager {
            storage,
            timers: TimerStorage {
                running: false,
                // the clock of `now()`, it may lag `Instant::now()`
                base: now(),
                current: 0,
                cache: VecDeque::with_capacity(CAP),
                notifications: BTreeMap::default(),
            },
            iops: Iops {
                running: false,
                waker: None,
                task: 0,
                ops: Vec::with_capacity(32),
            },
        }
    }
}

impl IoManager {
    /// Calls `f` with the current thread's manager.
    ///
    /// The manager is created on first use and dropped when the arbiter shuts
    /// down, so that its state does not carry over to the next runtime on the
    /// same thread. If the thread-local storage has already been destroyed
    /// because the thread is exiting, `f` receives a temporary manager.
    fn with<F, R>(f: F) -> R
    where
        F: FnOnce(&mut IoManager) -> R,
    {
        let mut f = Some(f);
        MANAGER
            .try_with(|cell| {
                let mut mgr = cell.borrow_mut();
                let mgr = mgr.get_or_insert_with(|| {
                    Arbiter::on_shutdown(IoManager::reset);
                    IoManager::default()
                });
                (f.take().unwrap())(mgr)
            })
            .unwrap_or_else(|_| (f.take().unwrap())(&mut IoManager::default()))
    }

    fn reset() {
        // dropped outside of the borrow, the registered streams it holds may
        // unregister themselves
        let mgr = MANAGER
            .try_with(|cell| cell.borrow_mut().take())
            .ok()
            .flatten();
        drop(mgr);
    }

    fn get(&self, id: Id) -> Option<&IoRef> {
        if let Some(id) = id.0 {
            self.storage.get(id.get()).and_then(|item| item.as_ref())
        } else {
            None
        }
    }

    pub(crate) fn register(io: &IoRef) -> Id {
        IoManager::with(|manager| {
            let entry = manager.storage.vacant_entry();
            let id = Id(NonZeroUsize::new(entry.key()));
            entry.insert(Some(io.clone()));
            id
        })
    }

    pub(crate) fn unregister(io: &IoRef) {
        if let Some(id) = io.id().0 {
            io.0.id.set(Id(None));
            IoManager::with(|manager| {
                // the manager may have been reset since the stream registered,
                // the id can belong to another stream then
                if let Some(Some(item)) = manager.storage.get(id.get())
                    && Rc::ptr_eq(&item.0, &io.0)
                {
                    manager.storage.remove(id.get());
                }
            });
        }
    }
}

pub(crate) struct Iops {
    running: bool,
    /// Waker of the task that runs scheduled writes.
    ///
    /// The task lives as long as the manager, waking it is cheaper than
    /// spawning a task for every batch.
    waker: Option<LocalWaker>,
    /// Id of the last spawned task, a dropped task that has been replaced
    /// must not clear the waker of its successor.
    task: u64,
    pub(crate) ops: Vec<Id>,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(mem::size_of::<Iops>() == 56);

impl Iops {
    pub(crate) fn schedule_write(id: Id) {
        IoManager::with(|mgr| {
            mgr.iops.ops.push(id);

            if !mgr.iops.running {
                mgr.iops.running = true;
                if let Some(waker) = &mgr.iops.waker {
                    waker.wake_by_ref();
                } else {
                    let task = IOPS_TASK
                        .try_with(|id| {
                            id.set(id.get().wrapping_add(1));
                            id.get()
                        })
                        .unwrap_or_default();
                    mgr.iops.task = task;
                    spawn_detached(Iops::task(task));
                }
            }
        });
    }

    async fn task(id: u64) {
        /// Clears the stored waker if the runtime drops the task, the next
        /// batch then spawns a new one.
        struct Guard(u64);

        impl Drop for Guard {
            fn drop(&mut self) {
                let waker = MANAGER.try_with(|cell| {
                    if let Ok(mut mgr) = cell.try_borrow_mut()
                        && let Some(mgr) = mgr.as_mut()
                        && mgr.iops.task == self.0
                    {
                        mgr.iops.running = false;
                        mgr.iops.waker.take()
                    } else {
                        None
                    }
                });
                drop(waker);
            }
        }

        let _guard = Guard(id);
        std::future::poll_fn(|cx| {
            Iops::run();
            let replaced = IoManager::with(|mgr| {
                if mgr.iops.task == id
                    && !mgr
                        .iops
                        .waker
                        .as_ref()
                        .is_some_and(|w| w.will_wake(cx.waker()))
                {
                    mgr.iops.waker.replace(LocalWaker::from_waker(cx.waker()))
                } else {
                    None
                }
            });
            drop(replaced);
            Poll::<()>::Pending
        })
        .await;
    }

    pub(crate) fn run() {
        IoManager::with(|mgr| {
            mgr.iops.running = false;

            let mut ops = mem::take(&mut mgr.iops.ops);
            for id in ops.drain(..) {
                if let Some(io) = mgr.get(id) {
                    io.ops_send_buf();
                }
            }
            let _ = mem::replace(&mut mgr.iops.ops, ops);
        });
    }

    #[cfg(test)]
    pub(crate) fn is_registered(io: &IoRef) -> bool {
        IoManager::with(|mgr| mgr.iops.ops.contains(&io.id()))
    }
}

#[cfg(test)]
mod tests {
    use ntex::rt::{DefaultRuntime, System};

    use super::*;

    async fn wait_timeout(io: &crate::Io) -> Duration {
        let start = Instant::now();
        let st = std::future::poll_fn(|cx| io.poll_status_update(cx)).await;
        assert!(matches!(st, crate::IoStatusUpdate::Timeout));
        start.elapsed()
    }

    #[test]
    fn large_timer_sets_are_not_cached() {
        let mut timers = TimerStorage {
            running: false,
            base: Instant::now(),
            current: 0,
            cache: VecDeque::new(),
            notifications: BTreeMap::new(),
        };

        let mut small = HashSet::default();
        small.insert(Id(NonZeroUsize::new(1)));
        small.clear();
        timers.recycle(small);
        assert_eq!(timers.cache.len(), 1);

        // a set grown by a burst of timers is dropped
        let mut large = HashSet::default();
        for i in 1..=10_000 {
            large.insert(Id(NonZeroUsize::new(i)));
        }
        large.clear();
        timers.recycle(large);
        assert_eq!(timers.cache.len(), 1);
        assert!(timers.cache[0].capacity() <= MAX_CACHED_SET_CAP);

        // the cache holds at most `CAP` sets
        for _ in 0..2 * CAP {
            timers.recycle(HashSet::default());
        }
        assert_eq!(timers.cache.len(), CAP);
    }

    /// A timer started while no timers are pending expires after its
    /// timeout, the ticker does not add a second.
    #[ntex::test]
    async fn timer_expires_after_timeout() {
        use ntex_service::cfg::SharedCfg;

        use crate::{Io, testing::IoTest};

        let (_client, server) = IoTest::create();
        let io = Io::new(server, SharedCfg::new("T"));

        // the clock is not aligned to the start of the runtime
        sleep(Millis(500)).await;

        io.start_timer(Seconds(1));
        let elapsed = wait_timeout(&io).await;
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_millis(1300),
            "elapsed: {elapsed:?}"
        );
    }

    /// A timer started while other timers are pending expires within a
    /// second after its timeout.
    #[ntex::test]
    async fn timer_expires_within_second_after_timeout() {
        use ntex_service::cfg::SharedCfg;

        use crate::{Io, testing::IoTest};

        let (_client1, server1) = IoTest::create();
        let (_client2, server2) = IoTest::create();
        let io1 = Io::new(server1, SharedCfg::new("T"));
        let io2 = Io::new(server2, SharedCfg::new("T"));

        io1.start_timer(Seconds(10));
        sleep(Millis(500)).await;

        // expires at the second clock second
        io2.start_timer(Seconds(1));
        let elapsed = wait_timeout(&io2).await;
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_millis(1800),
            "elapsed: {elapsed:?}"
        );
    }

    fn has_manager() -> bool {
        MANAGER.with(|mgr| mgr.borrow().is_some())
    }

    fn iops_state() -> (bool, u64, bool) {
        IoManager::with(|mgr| (mgr.iops.running, mgr.iops.task, mgr.iops.waker.is_some()))
    }

    #[ntex::test]
    async fn local_write_task_replacement_and_cleanup() {
        use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
        use std::task::{Context, Wake, Waker};

        struct Probe {
            notifications: Arc<AtomicUsize>,
            drops: Arc<AtomicUsize>,
            registered_on_drop: bool,
        }

        impl Wake for Probe {
            fn wake(self: Arc<Self>) {
                self.notifications.fetch_add(1, Ordering::Relaxed);
            }
        }

        impl Drop for Probe {
            fn drop(&mut self) {
                assert_eq!(iops_state(), (false, 1, self.registered_on_drop));
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        IoManager::with(|mgr| mgr.iops.task = 1);
        let mut task = Box::pin(Iops::task(1));
        let notifications = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let probe = |registered_on_drop| {
            Waker::from(Arc::new(Probe {
                notifications: notifications.clone(),
                drops: drops.clone(),
                registered_on_drop,
            }))
        };
        let waker = probe(true);
        assert!(
            task.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(waker);
        Iops::schedule_write(Id(None));
        assert_eq!(notifications.load(Ordering::Relaxed), 1);

        std::future::poll_fn(|cx| {
            assert!(task.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(drops.load(Ordering::Relaxed), 1);

        let waker = probe(false);
        assert!(
            task.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(waker);
        Iops::schedule_write(Id(None));
        assert_eq!(notifications.load(Ordering::Relaxed), 2);
        drop(task);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(iops_state(), (false, 1, false));

        IoManager::with(|mgr| mgr.iops.task = 2);
        let mut old = Box::pin(Iops::task(2));
        assert!(
            old.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        IoManager::with(|mgr| mgr.iops.task = 3);
        let mut new = Box::pin(Iops::task(3));
        std::future::poll_fn(|cx| {
            assert!(new.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        Iops::schedule_write(Id(None));
        drop(old);
        assert_eq!(iops_state(), (true, 3, true));
        drop(new);
        assert_eq!(iops_state(), (false, 3, false));
    }

    /// Write batches of later turns are run by the same task, a runtime that
    /// replaces the manager starts a new one.
    #[test]
    fn write_task_is_reused() {
        std::thread::spawn(|| {
            let first = System::new("test", DefaultRuntime).block_on(async {
                Iops::schedule_write(Id(None));
                sleep(Millis(5)).await;
                let (running, task, waker) = iops_state();
                assert!(!running && waker, "batch was not run");

                for _ in 0..3 {
                    Iops::schedule_write(Id(None));
                    Iops::schedule_write(Id(None));
                    assert!(iops_state().0);
                    sleep(Millis(5)).await;
                    assert_eq!(iops_state(), (false, task, true));
                }
                task
            });

            System::new("test", DefaultRuntime).block_on(async move {
                Iops::schedule_write(Id(None));
                sleep(Millis(5)).await;
                let (running, task, waker) = iops_state();
                assert!(!running && waker, "batch of the next runtime was not run");
                assert_ne!(task, first);
            });
        })
        .join()
        .unwrap();
    }

    /// The manager must not carry over to the next runtime on the thread, a
    /// write scheduled by a runtime that has stopped would block the writes of
    /// the next one.
    #[test]
    fn manager_reset_on_shutdown() {
        std::thread::spawn(|| {
            System::new("test", DefaultRuntime).block_on(async {
                Iops::schedule_write(Id(None));
                assert!(has_manager());
            });
            assert!(!has_manager());

            System::build()
                .build(DefaultRuntime)
                .run(|| {
                    Iops::schedule_write(Id(None));
                    System::current().stop();
                    Ok(())
                })
                .unwrap();
            assert!(!has_manager());
        })
        .join()
        .unwrap();
    }
}
