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

struct Inner {
    running: bool,
    cache: VecDeque<HashSet<Rc<IoState>>>,
    notifications: BTreeMap<Instant, HashSet<Rc<IoState>>>,
}

impl Inner {
    fn unregister(&mut self, expire: Instant, io: &IoRef) {
        if let Some(states) = self.notifications.get_mut(&expire) {
            states.remove(&io.0);
            if states.is_empty() {
                if let Some(items) = self.notifications.remove(&expire) {
                    if self.cache.len() <= CAP {
                        self.cache.push_back(items);
                    }
                }
            }
        }
    }
}

pub(crate) fn register(timeout: Duration, io: &IoRef) -> Instant {
    TIMER.with(|timer| {
        let mut inner = timer.borrow_mut();

        let expire = now() + timeout;

        // search existing key
        let expire = if let Some((expire, _)) =
            inner.notifications.range(expire..expire + SEC).next()
        {
            *expire
        } else {
            let items = inner.cache.pop_front().unwrap_or_default();
            inner.notifications.insert(expire, items);
            expire
        };

        inner
            .notifications
            .get_mut(&expire)
            .unwrap()
            .insert(io.0.clone());

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

pub(crate) fn unregister(expire: Instant, io: &IoRef) {
    TIMER.with(|timer| {
        timer.borrow_mut().unregister(expire, io);
    })
}
