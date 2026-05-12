#![allow(clippy::cast_possible_truncation)]
use std::collections::{BTreeMap, VecDeque};
use std::{cell::Cell, mem, num::NonZeroUsize, ops, time::Duration, time::Instant};

use ntex_rt::with_item;
use ntex_util::time::{Seconds, now, sleep};
use ntex_util::{HashSet, spawn};
use slab::Slab;

use crate::IoRef;

const CAP: usize = 64;
const SEC: Duration = Duration::from_secs(1);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Id(Option<NonZeroUsize>);

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TimerHandle(u32);

impl TimerHandle {
    pub const ZERO: TimerHandle = TimerHandle(0);

    pub fn is_set(&self) -> bool {
        self.0 != 0
    }

    pub fn remains(&self) -> Seconds {
        IoManager::with(|mgr| {
            let cur = mgr.timers.current;
            if self.0 <= cur {
                Seconds::ZERO
            } else {
                #[allow(clippy::cast_possible_truncation)]
                Seconds((self.0 - cur) as u16)
            }
        })
    }

    pub fn instant(&self) -> Instant {
        IoManager::with(|mgr| mgr.timers.base + Duration::from_secs(u64::from(self.0)))
    }

    pub(crate) fn update(self, timeout: Seconds, io: &IoRef) -> TimerHandle {
        IoManager::with(|mgr| {
            let new_hnd = mgr.timers.current + u32::from(timeout.0);
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
    current: u32,
    cache: VecDeque<HashSet<Id>>,
    notifications: BTreeMap<u32, HashSet<Id>>,
}

impl TimerStorage {
    fn unregister(&mut self, hnd: TimerHandle, io: &IoRef) {
        if let Some(states) = self.notifications.get_mut(&hnd.0) {
            states.remove(&io.id());
        }
    }

    fn register(&mut self, timeout: Seconds, io: &IoRef) -> TimerHandle {
        // setup current delta
        if !self.running {
            self.current = (now() - self.base).as_secs() as u32;
        }

        let hnd = {
            let hnd = self.current + u32::from(timeout.0);

            // insert key
            if let Some(item) = self.notifications.range_mut(hnd..=hnd).next() {
                item.1.insert(io.id());
                *item.0
            } else {
                let mut items = self.cache.pop_front().unwrap_or_default();
                items.insert(io.id());
                self.notifications.insert(hnd, items);
                hnd
            }
        };

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
                sleep(SEC).await;
                let stop = IoManager::with(|mgr| {
                    let current = mgr.timers.current;
                    mgr.timers.current = current + 1;

                    // notify io dispatcher
                    while let Some(key) = mgr.timers.notifications.keys().next() {
                        let key = *key;
                        if key <= current {
                            let mut items = mgr.timers.notifications.remove(&key).unwrap();
                            for id in items.drain() {
                                if let Some(io) = mgr.get(id) {
                                    io.notify_timeout();
                                }
                            }
                            if mgr.timers.cache.len() <= CAP {
                                mgr.timers.cache.push_back(items);
                            }
                        } else {
                            break;
                        }
                    }

                    // new tick
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

struct IoStorage(Cell<Option<Box<IoManager>>>);

pub(crate) struct IoManager {
    storage: Slab<Option<IoRef>>,
    timers: TimerStorage,
    pub(crate) iops: Iops,
}

impl Default for IoStorage {
    fn default() -> IoStorage {
        IoStorage(Cell::new(Some(Box::new(IoManager::default()))))
    }
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
    fn with<F, R>(f: F) -> R
    where
        F: FnOnce(&mut IoManager) -> R,
    {
        with_item::<IoStorage, _, _>(|st| {
            let mut mgr = st.0.take().unwrap();
            let result = f(&mut mgr);
            st.0.set(Some(mgr));
            result
        })
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
                if manager.storage.contains(id.get()) {
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
    pub(crate) fn register_send(id: Id) {
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
