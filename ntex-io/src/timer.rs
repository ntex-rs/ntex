use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration, time::Instant};

use ntex_util::time::{now, sleep, Millis};
use ntex_util::{spawn, HashSet};

use crate::{io::IoState, IoRef};

thread_local! {
    static TIMER: Rc<RefCell<Inner>> = Rc::new(RefCell::new(
        Inner {
            running: false,
            notifications: BTreeMap::default(),
        }));
}

struct Inner {
    running: bool,
    notifications: BTreeMap<Instant, HashSet<Rc<IoState>>>,
}

impl Inner {
    fn unregister(&mut self, expire: Instant, io: &IoRef) {
        if let Some(states) = self.notifications.get_mut(&expire) {
            states.remove(&io.0);
            if states.is_empty() {
                self.notifications.remove(&expire);
            }
        }
    }
}

pub(crate) fn register(timeout: Duration, io: &IoRef) -> Instant {
    let expire = now() + timeout;

    TIMER.with(|timer| {
        let mut inner = timer.borrow_mut();

        inner
            .notifications
            .entry(expire)
            .or_default()
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
                                for st in i.notifications.remove(&key).unwrap() {
                                    st.notify_keepalive();
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
    });

    expire
}

struct TimerGuard(Rc<RefCell<Inner>>);

impl Drop for TimerGuard {
    fn drop(&mut self) {
        self.0.borrow_mut().running = false;
    }
}

pub(crate) fn unregister(expire: Instant, io: &IoRef) {
    TIMER.with(|timer| {
        timer.borrow_mut().unregister(expire, io);
    })
}
