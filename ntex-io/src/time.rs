use std::{
    cell::RefCell, collections::BTreeMap, collections::HashSet, rc::Rc, time::Instant,
};

use ntex_util::spawn;
use ntex_util::time::{now, sleep, Millis};

use super::state::{Flags, IoRef, IoStateInner};

pub struct Timer(Rc<RefCell<Inner>>);

struct Inner {
    running: bool,
    resolution: Millis,
    notifications: BTreeMap<Instant, HashSet<Rc<IoStateInner>, fxhash::FxBuildHasher>>,
}

impl Inner {
    fn new(resolution: Millis) -> Self {
        Inner {
            resolution,
            running: false,
            notifications: BTreeMap::default(),
        }
    }

    fn unregister(&mut self, expire: Instant, io: &IoRef) {
        if let Some(states) = self.notifications.get_mut(&expire) {
            states.remove(&io.0);
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

    pub fn register(&self, expire: Instant, previous: Instant, io: &IoRef) {
        let mut inner = self.0.borrow_mut();

        inner.unregister(previous, io);
        inner
            .notifications
            .entry(expire)
            .or_insert_with(HashSet::default)
            .insert(io.0.clone());

        if !inner.running {
            inner.running = true;
            let interval = inner.resolution;
            let inner = self.0.clone();

            spawn(async move {
                loop {
                    sleep(interval).await;
                    {
                        let mut i = inner.borrow_mut();
                        let now_time = now();

                        // notify io dispatcher
                        while let Some(key) = i.notifications.keys().next() {
                            let key = *key;
                            if key <= now_time {
                                for st in i.notifications.remove(&key).unwrap() {
                                    st.dispatch_task.wake();
                                    st.insert_flags(Flags::DSP_KEEPALIVE);
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
            });
        }
    }

    pub fn unregister(&self, expire: Instant, io: &IoRef) {
        self.0.borrow_mut().unregister(expire, io);
    }
}
