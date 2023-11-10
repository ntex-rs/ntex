#![allow(clippy::mutable_key_type)]
use std::collections::{BTreeMap, VecDeque};
use std::{cell::RefCell, rc::Rc, time::Duration, time::Instant};

use ntex_util::time::{now, sleep, Millis};
use ntex_util::{spawn, HashSet};

use crate::{io::IoState, IoRef};

const CAP: usize = 64;
const SEC: Duration = Duration::from_secs(1);

thread_local! {
    static TIMER: Rc<RefCell<Inner>> = Rc::new(RefCell::new(
        Inner {
            running: false,
            cache: VecDeque::with_capacity(CAP),
            notifications: BTreeMap::default(),
        }));
}

type Notifications = BTreeMap<Instant, (HashSet<Rc<IoState>>, HashSet<Rc<IoState>>)>;

struct Inner {
    running: bool,
    cache: VecDeque<HashSet<Rc<IoState>>>,
    notifications: Notifications,
}

impl Inner {
    fn unregister(&mut self, expire: Instant, io: &IoRef, custom: bool) {
        if let Some(states) = self.notifications.get_mut(&expire) {
            if custom {
                states.1.remove(&io.0);
            } else {
                states.0.remove(&io.0);
            }
            if states.0.is_empty() && states.1.is_empty() {
                if let Some(items) = self.notifications.remove(&expire) {
                    if self.cache.len() <= CAP {
                        self.cache.push_back(items.0);
                        self.cache.push_back(items.1);
                    }
                }
            }
        }
    }
}

pub(crate) fn register(timeout: Duration, io: &IoRef, custom: bool) -> Instant {
    TIMER.with(|timer| {
        let mut inner = timer.borrow_mut();

        let expire = now() + timeout;

        // search existing key
        let expire = if let Some((expire, _)) =
            inner.notifications.range(expire..expire + SEC).next()
        {
            *expire
        } else {
            let n0 = inner.cache.pop_front().unwrap_or_default();
            let n1 = inner.cache.pop_front().unwrap_or_default();
            inner.notifications.insert(expire, (n0, n1));
            expire
        };

        let notifications = inner.notifications.get_mut(&expire).unwrap();
        if custom {
            notifications.1.insert(io.0.clone());
        } else {
            notifications.0.insert(io.0.clone());
        };

        if !inner.running {
            inner.running = true;
            let inner = timer.clone();

            spawn(async move {
                let guard = TimerGuard(inner.clone());
                loop {
                    sleep(Millis::ONE_SEC).await;
                    {
                        let mut i = inner.borrow_mut();
                        let now_time = now();

                        // notify io dispatcher
                        while let Some(key) = i.notifications.keys().next() {
                            let key = *key;
                            if key <= now_time {
                                let mut items = i.notifications.remove(&key).unwrap();
                                items.0.drain().for_each(|st| st.notify_timeout(false));
                                items.1.drain().for_each(|st| st.notify_timeout(true));
                                if i.cache.len() <= CAP {
                                    i.cache.push_back(items.0);
                                    i.cache.push_back(items.1);
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

        expire
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

pub(crate) fn unregister(expire: Instant, io: &IoRef, custom: bool) {
    TIMER.with(|timer| {
        timer.borrow_mut().unregister(expire, io, custom);
    })
}
