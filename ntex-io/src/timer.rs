#![allow(clippy::mutable_key_type)]
use std::collections::{BTreeMap, VecDeque};
use std::{cell::RefCell, ops, rc::Rc, time::Duration, time::Instant};

use ntex_util::time::{now, sleep, Seconds};
use ntex_util::{spawn, HashSet};

use crate::{io::IoState, IoRef};

const CAP: usize = 64;
const SEC: Duration = Duration::from_secs(1);

thread_local! {
    static TIMER: Rc<RefCell<Inner>> = Rc::new(RefCell::new(
        Inner {
            running: false,
            base: Instant::now(),
            current: 0,
            cache: VecDeque::with_capacity(CAP),
            notifications: BTreeMap::default(),
        }));
}

#[derive(Copy, Clone, Default, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TimerHandle(u32);

impl TimerHandle {
    pub fn remains(&self) -> Seconds {
        TIMER.with(|timer| {
            let cur = timer.borrow().current;
            if self.0 <= cur {
                Seconds::ZERO
            } else {
                Seconds((self.0 - cur) as u16)
            }
        })
    }

    pub fn instant(&self) -> Instant {
        TIMER.with(|timer| timer.borrow().base + Duration::from_secs(self.0 as u64))
    }
}

impl ops::Add<Seconds> for TimerHandle {
    type Output = TimerHandle;

    #[inline]
    fn add(self, other: Seconds) -> TimerHandle {
        TimerHandle(self.0 + other.0 as u32)
    }
}

struct Inner {
    running: bool,
    base: Instant,
    current: u32,
    cache: VecDeque<HashSet<Rc<IoState>>>,
    notifications: BTreeMap<u32, HashSet<Rc<IoState>>>,
}

impl Inner {
    fn unregister(&mut self, hnd: TimerHandle, io: &IoRef) {
        if let Some(states) = self.notifications.get_mut(&hnd.0) {
            states.remove(&io.0);
            if states.is_empty() {
                if let Some(items) = self.notifications.remove(&hnd.0) {
                    if self.cache.len() <= CAP {
                        self.cache.push_back(items);
                    }
                }
            }
        }
    }
}

pub(crate) fn register(timeout: Seconds, io: &IoRef) -> TimerHandle {
    TIMER.with(|timer| {
        let mut inner = timer.borrow_mut();

        // setup current delta
        if !inner.running {
            inner.current = (now() - inner.base).as_secs() as u32;
        }

        let hnd = inner.current + timeout.0 as u32;

        // search existing key
        let hnd = if let Some((hnd, _)) = inner.notifications.range(hnd..hnd + 1).next() {
            *hnd
        } else {
            let items = inner.cache.pop_front().unwrap_or_default();
            inner.notifications.insert(hnd, items);
            hnd
        };

        inner
            .notifications
            .get_mut(&hnd)
            .unwrap()
            .insert(io.0.clone());

        if !inner.running {
            inner.running = true;
            let inner = timer.clone();

            spawn(async move {
                let guard = TimerGuard(inner.clone());
                loop {
                    sleep(SEC).await;
                    {
                        let mut i = inner.borrow_mut();
                        i.current += 1;

                        // notify io dispatcher
                        while let Some(key) = i.notifications.keys().next() {
                            let key = *key;
                            if key <= i.current {
                                let mut items = i.notifications.remove(&key).unwrap();
                                items.drain().for_each(|st| st.notify_timeout());
                                if i.cache.len() <= CAP {
                                    i.cache.push_back(items);
                                }
                            } else {
                                break;
                            }
                        }

                        // new tick
                        if i.notifications.is_empty() {
                            i.running = false;
                            break;
                        }
                    }
                }
                drop(guard);
            });
        }

        TimerHandle(hnd)
    })
}

struct TimerGuard(Rc<RefCell<Inner>>);

impl Drop for TimerGuard {
    fn drop(&mut self) {
        let mut inner = self.0.borrow_mut();
        inner.running = false;
        inner.notifications.clear();
    }
}

pub(crate) fn unregister(hnd: TimerHandle, io: &IoRef) {
    TIMER.with(|timer| {
        timer.borrow_mut().unregister(hnd, io);
    })
}
