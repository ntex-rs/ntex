#![allow(clippy::mutable_key_type)]
use std::collections::{BTreeMap, VecDeque};
use std::{cell::Cell, cell::RefCell, ops, rc::Rc, time::Duration, time::Instant};

use ntex_util::time::{Seconds, now, sleep};
use ntex_util::{HashSet, spawn};

use crate::{IoRef, io::IoState};

const CAP: usize = 64;
const SEC: Duration = Duration::from_secs(1);

thread_local! {
    static TIMER: Inner = Inner {
        running: Cell::new(false),
        base: Cell::new(Instant::now()),
        current: Cell::new(0),
        storage: RefCell::new(InnerMut {
            cache: VecDeque::with_capacity(CAP),
            notifications: BTreeMap::default(),
        })
    }
}

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TimerHandle(u32);

impl TimerHandle {
    pub const ZERO: TimerHandle = TimerHandle(0);

    pub fn is_set(&self) -> bool {
        self.0 != 0
    }

    pub fn remains(&self) -> Seconds {
        TIMER.with(|timer| {
            let cur = timer.current.get();
            if self.0 <= cur {
                Seconds::ZERO
            } else {
                #[allow(clippy::cast_possible_truncation)]
                Seconds((self.0 - cur) as u16)
            }
        })
    }

    pub fn instant(&self) -> Instant {
        TIMER.with(|timer| timer.base.get() + Duration::from_secs(u64::from(self.0)))
    }
}

impl ops::Add<Seconds> for TimerHandle {
    type Output = TimerHandle;

    #[inline]
    fn add(self, other: Seconds) -> TimerHandle {
        TimerHandle(self.0 + u32::from(other.0))
    }
}

struct Inner {
    running: Cell<bool>,
    base: Cell<Instant>,
    current: Cell<u32>,
    storage: RefCell<InnerMut>,
}

struct InnerMut {
    cache: VecDeque<HashSet<Rc<IoState>>>,
    notifications: BTreeMap<u32, HashSet<Rc<IoState>>>,
}

impl InnerMut {
    fn unregister(&mut self, hnd: TimerHandle, io: &IoRef) {
        if let Some(states) = self.notifications.get_mut(&hnd.0) {
            states.remove(&io.0);
        }
    }
}

pub(crate) fn unregister(hnd: TimerHandle, io: &IoRef) {
    TIMER.with(|timer| {
        timer.storage.borrow_mut().unregister(hnd, io);
    });
}

pub(crate) fn update(hnd: TimerHandle, timeout: Seconds, io: &IoRef) -> TimerHandle {
    TIMER.with(|timer| {
        let new_hnd = timer.current.get() + u32::from(timeout.0);
        if hnd.0 == new_hnd || hnd.0 == new_hnd + 1 {
            hnd
        } else {
            timer.storage.borrow_mut().unregister(hnd, io);
            register(timeout, io)
        }
    })
}

pub(crate) fn register(timeout: Seconds, io: &IoRef) -> TimerHandle {
    TIMER.with(|timer| {
        // setup current delta
        if !timer.running.get() {
            #[allow(clippy::cast_possible_truncation)]
            let current = (now() - timer.base.get()).as_secs() as u32;
            timer.current.set(current);
            log::debug!(
                "{}: Timer driver does not run, current: {}",
                io.tag(),
                current
            );
        }

        let hnd = {
            let hnd = timer.current.get() + u32::from(timeout.0);
            let mut inner = timer.storage.borrow_mut();

            // insert key
            if let Some(item) = inner.notifications.range_mut(hnd..=hnd).next() {
                item.1.insert(io.0.clone());
                *item.0
            } else {
                let mut items = inner.cache.pop_front().unwrap_or_default();
                items.insert(io.0.clone());
                inner.notifications.insert(hnd, items);
                hnd
            }
        };

        if !timer.running.get() {
            timer.running.set(true);

            spawn(async move {
                let guard = TimerGuard;
                loop {
                    sleep(SEC).await;
                    let stop = TIMER.with(|timer| {
                        let current = timer.current.get();
                        timer.current.set(current + 1);

                        // notify io dispatcher
                        let mut inner = timer.storage.borrow_mut();
                        while let Some(key) = inner.notifications.keys().next() {
                            let key = *key;
                            if key <= current {
                                let mut items = inner.notifications.remove(&key).unwrap();
                                for st in items.drain() {
                                    st.notify_timeout();
                                }
                                if inner.cache.len() <= CAP {
                                    inner.cache.push_back(items);
                                }
                            } else {
                                break;
                            }
                        }

                        // new tick
                        if inner.notifications.is_empty() {
                            timer.running.set(false);
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

        TimerHandle(hnd)
    })
}

struct TimerGuard;

impl Drop for TimerGuard {
    fn drop(&mut self) {
        TIMER.with(|timer| {
            timer.running.set(false);
            timer.storage.borrow_mut().notifications.clear();
        });
    }
}
