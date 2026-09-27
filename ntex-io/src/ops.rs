#![allow(clippy::cast_possible_truncation)]
use std::collections::{BTreeMap, VecDeque};
use std::{cell::RefCell, mem, num::NonZeroUsize, ops, rc::Rc, time::Duration, time::Instant};

use ntex_rt::Arbiter;
use ntex_util::time::{Millis, Seconds, now, sleep};
use ntex_util::{HashSet, spawn};
use slab::Slab;

use crate::IoRef;

const CAP: usize = 64;

thread_local! {
    static MANAGER: RefCell<Option<IoManager>> = const { RefCell::new(None) };
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
                if self.cache.len() < CAP {
                    self.cache.push_back(items);
                }
            }
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

        spawn(async move {
            let guard = TimerGuard;
            loop {
                // tick at the next whole second of the clock, a late tick
                // does not delay the later ones
                let next = IoManager::with(|mgr| {
                    let t = &mgr.timers;
                    let next = t.base + Duration::from_secs(u64::from(t.current) + 1);
                    next.saturating_duration_since(Instant::now())
                });
                sleep(Millis(next.as_millis() as u32 + 1)).await;

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
                        if mgr.timers.cache.len() < CAP {
                            mgr.timers.cache.push_back(items);
                        }
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
            }
            drop(guard);
        });
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
                base: Instant::now(),
                current: 0,
                cache: VecDeque::with_capacity(CAP),
                notifications: BTreeMap::default(),
            },
            iops: Iops {
                running: false,
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
    pub(crate) ops: Vec<Id>,
}

impl Iops {
    pub(crate) fn schedule_write(id: Id) {
        IoManager::with(|mgr| {
            mgr.iops.ops.push(id);

            if !mgr.iops.running {
                mgr.iops.running = true;
                spawn(async move { Iops::run() });
            }
        });
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
