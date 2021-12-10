use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Instant};

use crate::time::{now, sleep, Millis};
use crate::util::HashSet;

use super::state::{Flags, IoState, IoStateInner};

pub struct Timer(Rc<RefCell<Inner>>);

struct Inner {
    resolution: Millis,
    current: Option<Instant>,
    notifications: BTreeMap<Instant, HashSet<Rc<IoStateInner>>>,
}

impl Inner {
    fn new(resolution: Millis) -> Self {
        Inner {
            resolution,
            current: None,
            notifications: BTreeMap::default(),
        }
    }

    fn unregister(&mut self, expire: Instant, state: &IoState) {
        if let Some(states) = self.notifications.get_mut(&expire) {
            states.remove(&state.0);
            if states.is_empty() {
                self.notifications.remove(&expire);
            }
        }
    }
}

impl Clone for Timer {
    fn clone(&self) -> Self {
        Timer(self.0.clone())
    }
}

impl Default for Timer {
    fn default() -> Self {
        Timer::new(Millis::ONE_SEC)
    }
}

impl Timer {
    /// Create new timer with resolution in milliseconds
    pub fn new(resolution: Millis) -> Timer {
        Timer(Rc::new(RefCell::new(Inner::new(resolution))))
    }

    pub fn register(&self, expire: Instant, previous: Instant, state: &IoState) {
        {
            let mut inner = self.0.borrow_mut();

            inner.unregister(previous, state);
            inner
                .notifications
                .entry(expire)
                .or_insert_with(HashSet::default)
                .insert(state.0.clone());
        }

        let _ = self.now();
    }

    pub fn unregister(&self, expire: Instant, state: &IoState) {
        self.0.borrow_mut().unregister(expire, state);
    }

    /// Get current time. This function has to be called from
    /// future's poll method, otherwise it panics.
    pub fn now(&self) -> Instant {
        let cur = self.0.borrow().current;
        if let Some(cur) = cur {
            cur
        } else {
            let now_val = now();
            let inner = self.0.clone();
            let interval = {
                let mut b = inner.borrow_mut();
                b.current = Some(now_val);
                b.resolution
            };

            crate::rt::spawn(async move {
                sleep(interval).await;
                let empty = {
                    let mut i = inner.borrow_mut();
                    let now = i.current.take().unwrap_or_else(now);

                    // notify io dispatcher
                    while let Some(key) = i.notifications.keys().next() {
                        let key = *key;
                        if key <= now {
                            for st in i.notifications.remove(&key).unwrap() {
                                st.dispatch_task.wake();
                                st.insert_flags(Flags::DSP_KEEPALIVE);
                            }
                        } else {
                            break;
                        }
                    }
                    i.notifications.is_empty()
                };

                // extra tick
                if !empty {
                    let _ = Timer(inner).now();
                }
            });

            now_val
        }
    }
}
