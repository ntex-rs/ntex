use std::os::fd::{AsRawFd, RawFd};
use std::{cell::Cell, cell::RefCell, io, net, rc::Rc, task::Context, task::Poll, task::Waker};

use ntex_rt::Arbiter;
use slab::Slab;

use super::{Event, Handler, Key, Reactor, ReactorApi};
use crate::{Listener, ListenerOps};

#[derive(Clone)]
pub(crate) struct AcceptOps(Rc<AcceptOpsInner>);

struct AcceptOpsHandler {
    inner: Rc<AcceptOpsInner>,
}

struct Entry {
    generation: u16,
    waker: Option<Waker>,
    err: Option<io::Error>,
    /// The socket is registered in the poller
    attached: bool,
    /// Readable interest is armed and its event has not arrived yet
    armed: bool,
}

struct AcceptOpsInner {
    api: ReactorApi,
    entries: RefCell<Slab<Entry>>,
    /// Generation of the next listener
    generation: Cell<u16>,
}

struct TcpListener {
    sock: net::TcpListener,
    key: Key,
    ops: Rc<AcceptOpsInner>,
}

impl AcceptOps {
    pub(crate) fn get(driver: &Reactor) -> Self {
        Arbiter::get_value(move || {
            let mut inner = None;
            driver.register(|api| {
                let ops = Rc::new(AcceptOpsInner {
                    api,
                    entries: RefCell::new(Slab::new()),
                    generation: Cell::new(0),
                });
                inner = Some(ops.clone());
                Box::new(AcceptOpsHandler { inner: ops })
            });
            AcceptOps(inner.unwrap())
        })
    }

    pub(crate) fn listener(&self, sock: net::TcpListener) -> Listener {
        let generation = self.0.generation.get();
        self.0.generation.set(generation.wrapping_add(1));

        let idx = self.0.entries.borrow_mut().insert(Entry {
            generation,
            waker: None,
            err: None,
            attached: false,
            armed: false,
        });
        let idx = u32::try_from(idx).expect("Too many listeners");

        Listener::new(TcpListener {
            sock,
            key: Key::new(idx, generation),
            ops: self.0.clone(),
        })
    }
}

impl TcpListener {
    fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

impl ListenerOps for TcpListener {
    fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(net::TcpStream, net::SocketAddr)>> {
        loop {
            match self.sock.accept() {
                Ok(res) => return Poll::Ready(Ok(res)),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Poll::Ready(Err(err)),
            }
        }

        let mut entries = self.ops.entries.borrow_mut();
        let Some(entry) = entries
            .get_mut(self.key.idx as usize)
            .filter(|e| e.generation == self.key.generation)
        else {
            return Poll::Ready(Err(io::Error::other("reactor is stopped")));
        };

        if let Some(err) = entry.err.take() {
            return Poll::Ready(Err(err));
        }

        if !entry.armed {
            entry.armed = true;
            if entry.attached {
                self.ops
                    .api
                    .modify(self.fd(), self.key.id(), Event::readable(0));
            } else {
                entry.attached = true;
                self.ops
                    .api
                    .attach(self.fd(), self.key.id(), Event::readable(0));
            }
        }

        match entry.waker {
            Some(ref w) if w.will_wake(cx.waker()) => {}
            _ => entry.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<net::SocketAddr> {
        self.sock.local_addr()
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let mut entries = self.ops.entries.borrow_mut();
        let idx = self.key.idx as usize;
        if entries
            .get(idx)
            .is_some_and(|e| e.generation == self.key.generation)
        {
            // detach before the socket is closed, its fd number can be reused
            if entries.remove(idx).attached {
                self.ops.api.detach(self.fd(), self.key.id());
            }
        }
    }
}

impl AcceptOpsHandler {
    fn entry<F: FnOnce(&mut Entry)>(&self, id: usize, f: F) {
        let key = Key::from_id(id);
        let mut entries = self.inner.entries.borrow_mut();
        if let Some(entry) = entries
            .get_mut(key.idx as usize)
            .filter(|e| e.generation == key.generation)
        {
            f(entry);
            if let Some(waker) = entry.waker.take() {
                waker.wake();
            }
        }
    }
}

impl Handler for AcceptOpsHandler {
    fn event(&mut self, id: usize, _: Event) {
        self.entry(id, |entry| entry.armed = false);
    }

    fn error(&mut self, id: usize, err: io::Error) {
        log::trace!("Accept {id:?} is failed {err:?}");
        self.entry(id, |entry| {
            entry.armed = false;
            entry.err = Some(err);
        });
    }

    fn tick(&mut self) {}

    fn cleanup(&mut self) {
        self.inner.entries.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
    use std::task::Wake;

    use super::*;

    struct Wakes(AtomicUsize);

    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn setup() -> (Reactor, AcceptOps, AcceptOpsHandler) {
        let reactor = Reactor::new().unwrap();
        let mut inner = None;
        reactor.register(|api| {
            let ops = Rc::new(AcceptOpsInner {
                api,
                entries: RefCell::new(Slab::new()),
                generation: Cell::new(0),
            });
            inner = Some(ops.clone());
            Box::new(AcceptOpsHandler { inner: ops })
        });
        let inner = inner.unwrap();
        let handler = AcceptOpsHandler {
            inner: inner.clone(),
        };
        (reactor, AcceptOps(inner), handler)
    }

    fn listen() -> net::TcpListener {
        let sock = net::TcpListener::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        sock
    }

    fn id(ops: &AcceptOps) -> usize {
        let entries = ops.0.entries.borrow();
        let (idx, entry) = entries.iter().next().unwrap();
        Key::new(u32::try_from(idx).unwrap(), entry.generation).id() as usize
    }

    /// STATEMENT: A reactor error for a pending accept wakes the task and is
    /// returned by the next accept, which then waits again.
    ///
    /// HOW:
    /// - Poll an accept on a listener without connections, so its readable
    ///   interest is attached and armed.
    /// - Deliver an error for its id, check the waker is called and the entry
    ///   is disarmed.
    /// - Poll again and check the error is returned, then check the following
    ///   poll is pending and armed again.
    #[test]
    fn reactor_error_is_returned_once() {
        let (_reactor, ops, mut handler) = setup();
        let mut listener = ops.listener(listen());
        let woken = Arc::new(Wakes(AtomicUsize::new(0)));
        let waker = Waker::from(woken.clone());
        let mut cx = Context::from_waker(&waker);

        assert!(listener.0.poll_accept(&mut cx).is_pending());
        let id = id(&ops);
        assert!(ops.0.entries.borrow().iter().all(|(_, e)| e.armed));

        handler.error(id, io::Error::other("poll failed"));
        assert_eq!(woken.0.load(Ordering::SeqCst), 1);
        assert!(ops.0.entries.borrow().iter().all(|(_, e)| !e.armed));

        let Poll::Ready(Err(err)) = listener.0.poll_accept(&mut cx) else {
            panic!("expected the reactor error");
        };
        assert_eq!(err.to_string(), "poll failed");
        assert!(listener.0.poll_accept(&mut cx).is_pending());
        assert!(
            ops.0
                .entries
                .borrow()
                .iter()
                .all(|(_, e)| e.armed && e.attached)
        );
    }

    /// STATEMENT: Events and errors for a released listener do not reach a
    /// new listener that reuses its slot.
    ///
    /// HOW:
    /// - Create a listener, record its id and drop it.
    /// - Create a second listener, check it reuses the slot with another
    ///   generation and poll it so a waker is stored.
    /// - Deliver an event and an error with the old id and check the waker is
    ///   not called and no error is stored.
    #[test]
    fn stale_events_are_ignored() {
        let (_reactor, ops, mut handler) = setup();
        let old = {
            let _listener = ops.listener(listen());
            id(&ops)
        };
        assert!(ops.0.entries.borrow().is_empty());

        let mut listener = ops.listener(listen());
        let new = id(&ops);
        assert_ne!(old, new);
        assert_eq!(Key::from_id(old).idx, Key::from_id(new).idx);

        let woken = Arc::new(Wakes(AtomicUsize::new(0)));
        let waker = Waker::from(woken.clone());
        assert!(
            listener
                .0
                .poll_accept(&mut Context::from_waker(&waker))
                .is_pending()
        );

        handler.event(old, Event::readable(0));
        handler.error(old, io::Error::other("stale"));
        assert_eq!(woken.0.load(Ordering::SeqCst), 0);
        assert!(
            ops.0
                .entries
                .borrow()
                .iter()
                .all(|(_, e)| e.armed && e.err.is_none())
        );
    }

    /// STATEMENT: After the reactor is cleaned up, accept fails and dropping
    /// the listener does not touch the released registration.
    ///
    /// HOW:
    /// - Create a listener and poll it so its socket is attached.
    /// - Run handler cleanup, which clears all entries.
    /// - Check that accept returns the "reactor is stopped" error, then drop the
    ///   listener and check no entry is created.
    #[test]
    fn stopped_reactor_fails_accept() {
        let (_reactor, ops, mut handler) = setup();
        let mut listener = ops.listener(listen());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(listener.0.poll_accept(&mut cx).is_pending());

        handler.cleanup();
        let Poll::Ready(Err(err)) = listener.0.poll_accept(&mut cx) else {
            panic!("expected an error");
        };
        assert_eq!(err.to_string(), "reactor is stopped");
        drop(listener);
        assert!(ops.0.entries.borrow().is_empty());
    }
}
