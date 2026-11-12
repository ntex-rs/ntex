use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::{cell::RefCell, io, mem, net, rc::Rc, task::Context, task::Poll, task::Waker};

use ntex_io_uring::{opcode, types::Fd};
use ntex_rt::Arbiter;
use slab::Slab;
use socket2::{SockAddr, SockAddrStorage};

use super::{Handler, Reactor, ReactorApi};
use crate::{Listener, ListenerOps};

type AcceptResult = io::Result<(net::TcpStream, net::SocketAddr)>;

#[derive(Clone)]
pub(crate) struct AcceptOps(Rc<AcceptOpsInner>);

struct AcceptOpsHandler {
    inner: Rc<AcceptOpsInner>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// `Accept` is submitted
    Accept,
    /// `PollAdd` is submitted, the listener reported `EAGAIN`
    Poll,
}

/// Accept state, owned by the slab until no operation is in flight
struct Entry {
    /// The socket stays open until the kernel is done with it
    sock: net::TcpListener,
    /// Peer address, written by the kernel
    addr: Box<(libc::sockaddr_storage, libc::socklen_t)>,
    state: State,
    result: Option<AcceptResult>,
    waker: Option<Waker>,
    /// The listener is dropped, the entry is released on completion
    dropped: bool,
}

struct AcceptOpsInner {
    api: ReactorApi,
    supported: bool,
    entries: RefCell<Slab<Entry>>,
}

struct TcpListener {
    id: u32,
    ops: Rc<AcceptOpsInner>,
}

impl AcceptOps {
    pub(crate) fn get(reactor: &Reactor) -> Self {
        Arbiter::get_value(move || {
            let mut inner = None;
            reactor.register(|api| {
                let supported = api.is_supported(opcode::Accept::CODE)
                    && api.is_supported(opcode::PollAdd::CODE);
                let ops = Rc::new(AcceptOpsInner {
                    api,
                    supported,
                    entries: RefCell::new(Slab::new()),
                });
                inner = Some(ops.clone());
                Box::new(AcceptOpsHandler { inner: ops })
            });
            AcceptOps(inner.unwrap())
        })
    }

    pub(crate) fn listener(&self, sock: net::TcpListener) -> io::Result<Listener> {
        if !self.0.supported {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io-uring accept is not supported by the kernel",
            ));
        }

        let id = self.0.entries.borrow_mut().insert(Entry {
            sock,
            // SAFETY: all zeros is a valid `sockaddr_storage`
            addr: Box::new((unsafe { mem::zeroed() }, 0)),
            state: State::Idle,
            result: None,
            waker: None,
            dropped: false,
        });
        let id = u32::try_from(id).expect("Too many listeners");

        Ok(Listener::new(TcpListener {
            id,
            ops: self.0.clone(),
        }))
    }
}

impl AcceptOpsInner {
    fn submit_accept(&self, id: u32, entry: &mut Entry) {
        let fd = entry.sock.as_raw_fd();
        entry.state = State::Accept;
        entry.addr.1 = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let addr = (&raw mut entry.addr.0).cast::<libc::sockaddr>();
        let len = &raw mut entry.addr.1;

        // SAFETY: the slab owns the boxed address and the socket until the
        // operation completes
        unsafe {
            self.api.submit(id, move |en| {
                opcode::Accept::new(en, Fd(fd), addr, len).flags(libc::SOCK_CLOEXEC);
            });
        }
    }

    fn submit_poll(&self, id: u32, entry: &mut Entry) {
        let fd = entry.sock.as_raw_fd();
        entry.state = State::Poll;

        // SAFETY: the slab owns the socket until the operation completes
        unsafe {
            self.api.submit(id, move |en| {
                opcode::PollAdd::new(en, Fd(fd), libc::POLLIN as u32);
            });
        }
    }
}

impl ListenerOps for TcpListener {
    fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<AcceptResult> {
        let mut entries = self.ops.entries.borrow_mut();
        let Some(entry) = entries.get_mut(self.id as usize) else {
            return Poll::Ready(Err(io::Error::other("reactor is stopped")));
        };

        if let Some(res) = entry.result.take() {
            return Poll::Ready(res);
        }
        if entry.state == State::Idle {
            self.ops.submit_accept(self.id, entry);
        }

        match entry.waker {
            Some(ref w) if w.will_wake(cx.waker()) => {}
            _ => entry.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<net::SocketAddr> {
        self.ops
            .entries
            .borrow()
            .get(self.id as usize)
            .ok_or_else(|| io::Error::other("reactor is stopped"))?
            .sock
            .local_addr()
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let id = self.id as usize;
        let mut entries = self.ops.entries.borrow_mut();
        if let Some(entry) = entries.get_mut(id) {
            if entry.state == State::Idle {
                entries.remove(id);
            } else {
                // the completion releases the entry and closes the socket
                entry.dropped = true;
                entry.waker = None;
                entry.result = None;
                self.ops.api.cancel(self.id);
            }
        }
    }
}

fn accepted(fd: RawFd, addr: &(libc::sockaddr_storage, libc::socklen_t)) -> AcceptResult {
    // SAFETY: the kernel returned a new connected socket owned by us
    let stream = unsafe { net::TcpStream::from_raw_fd(fd) };

    let mut storage = SockAddrStorage::zeroed();
    // SAFETY: `sockaddr_storage` is a platform socket address type, the
    // kernel initialized `addr.1` bytes of it
    let addr = unsafe {
        *storage.view_as::<libc::sockaddr_storage>() = addr.0;
        SockAddr::new(storage, addr.1)
    };
    match addr.as_socket() {
        Some(addr) => Ok((stream, addr)),
        None => Err(io::Error::other("accepted socket has non-inet address")),
    }
}

impl Handler for AcceptOpsHandler {
    fn canceled(&mut self, id: usize) {
        log::trace!("accept-op is canceled {id:?}");
        let mut entries = self.inner.entries.borrow_mut();
        if let Some(entry) = entries.get_mut(id) {
            entry.state = State::Idle;
            if entry.dropped {
                entries.remove(id);
            } else if let Some(waker) = entry.waker.take() {
                waker.wake();
            }
        }
    }

    fn completed(&mut self, id: usize, _: u32, result: io::Result<usize>) {
        log::trace!("accept-op is completed {id:?} result: {result:?}");
        let mut entries = self.inner.entries.borrow_mut();
        let Some(entry) = entries.get_mut(id) else {
            return;
        };
        let state = mem::replace(&mut entry.state, State::Idle);

        let res = match result {
            Ok(fd) if state == State::Accept => accepted(fd as RawFd, &entry.addr),
            // the listener is readable
            Ok(_) => {
                if entry.dropped {
                    entries.remove(id);
                } else {
                    self.inner.submit_accept(id as u32, entry);
                }
                return;
            }
            Err(err) if !entry.dropped && err.kind() == io::ErrorKind::Interrupted => {
                self.inner.submit_accept(id as u32, entry);
                return;
            }
            // a non-blocking listener fails instead of waiting on older kernels
            Err(err)
                if !entry.dropped
                    && state == State::Accept
                    && err.kind() == io::ErrorKind::WouldBlock =>
            {
                self.inner.submit_poll(id as u32, entry);
                return;
            }
            Err(err) => Err(err),
        };

        if entry.dropped {
            // closes the accepted stream and the listener
            entries.remove(id);
        } else {
            entry.result = Some(res);
            if let Some(waker) = entry.waker.take() {
                waker.wake();
            }
        }
    }

    fn tick(&mut self) {}

    fn in_flight(&self) -> usize {
        self.inner
            .entries
            .borrow()
            .iter()
            .filter(|(_, e)| e.state != State::Idle)
            .count()
    }

    fn cleanup(&mut self) {
        self.inner.entries.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::IntoRawFd;
    use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
    use std::task::Wake;

    use super::*;

    type Finish = fn(&mut AcceptOpsHandler);

    struct Wakes(AtomicUsize);

    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Wakes {
        fn get(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// The reactor is never run, submitted operations stay in its queue.
    fn setup() -> Option<(Reactor, AcceptOps, AcceptOpsHandler)> {
        setup_with(true)
    }

    fn setup_with(supported: bool) -> Option<(Reactor, AcceptOps, AcceptOpsHandler)> {
        let reactor = Reactor::new(8).ok()?;
        let mut inner = None;
        reactor.register(|api| {
            let ops = Rc::new(AcceptOpsInner {
                api,
                supported,
                entries: RefCell::new(Slab::new()),
            });
            inner = Some(ops.clone());
            Box::new(AcceptOpsHandler { inner: ops })
        });
        let inner = inner.unwrap();
        let handler = AcceptOpsHandler {
            inner: inner.clone(),
        };
        Some((reactor, AcceptOps(inner), handler))
    }

    fn listener(ops: &AcceptOps) -> Listener {
        let sock = net::TcpListener::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        ops.listener(sock).unwrap()
    }

    fn state(ops: &AcceptOps) -> Option<State> {
        ops.0.entries.borrow().get(0).map(|e| e.state)
    }

    fn waker() -> (Arc<Wakes>, Waker) {
        let woken = Arc::new(Wakes(AtomicUsize::new(0)));
        (woken.clone(), Waker::from(woken))
    }

    /// A raw fd of a new socket, standing in for an accepted connection.
    fn socket_fd() -> usize {
        let sock = net::TcpListener::bind("127.0.0.1:0").unwrap();
        usize::try_from(sock.into_raw_fd()).unwrap()
    }

    /// STATEMENT: An accept that fails with `EAGAIN` waits for the listener to
    /// become readable and retries, and an interrupted accept is retried.
    ///
    /// HOW:
    /// - Poll an accept and check an `Accept` is in flight.
    /// - Complete it with `WouldBlock` and check a `PollAdd` is in flight.
    /// - Complete the poll and check an `Accept` is in flight again, then
    ///   complete it with `Interrupted` and check it is resubmitted.
    /// - Check the waker is never called and no result is stored.
    #[test]
    fn would_block_falls_back_to_poll() {
        let Some((_reactor, ops, mut handler)) = setup() else {
            return;
        };
        let mut listener = listener(&ops);
        let (woken, waker) = waker();
        let mut cx = Context::from_waker(&waker);

        assert!(listener.0.poll_accept(&mut cx).is_pending());
        assert_eq!(state(&ops), Some(State::Accept));
        assert_eq!(handler.in_flight(), 1);

        handler.completed(0, 0, Err(io::ErrorKind::WouldBlock.into()));
        assert_eq!(state(&ops), Some(State::Poll));

        handler.completed(0, 0, Ok(1));
        assert_eq!(state(&ops), Some(State::Accept));

        handler.completed(0, 0, Err(io::ErrorKind::Interrupted.into()));
        assert_eq!(state(&ops), Some(State::Accept));

        assert_eq!(woken.get(), 0);
        assert!(ops.0.entries.borrow()[0].result.is_none());
        assert!(listener.0.poll_accept(&mut cx).is_pending());
    }

    /// STATEMENT: A failed accept wakes the task and its error is returned by
    /// the next accept, which then submits a new operation.
    ///
    /// HOW:
    /// - Poll an accept so a waker is stored and an `Accept` is in flight.
    /// - Complete it with `ConnectionAborted` and check the waker is called
    ///   and nothing is in flight.
    /// - Poll again and check the error is returned, then check the following
    ///   poll submits a new `Accept`.
    #[test]
    fn accept_error_is_returned() {
        let Some((_reactor, ops, mut handler)) = setup() else {
            return;
        };
        let mut listener = listener(&ops);
        let (woken, waker) = waker();
        let mut cx = Context::from_waker(&waker);

        assert!(listener.0.poll_accept(&mut cx).is_pending());
        handler.completed(0, 0, Err(io::ErrorKind::ConnectionAborted.into()));
        assert_eq!(woken.get(), 1);
        assert_eq!(handler.in_flight(), 0);

        let Poll::Ready(Err(err)) = listener.0.poll_accept(&mut cx) else {
            panic!("expected an error");
        };
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
        assert!(listener.0.poll_accept(&mut cx).is_pending());
        assert_eq!(state(&ops), Some(State::Accept));
    }

    /// STATEMENT: A canceled accept of a live listener wakes the task, and the
    /// next accept submits a new operation.
    ///
    /// HOW:
    /// - Poll an accept so a waker is stored and an `Accept` is in flight.
    /// - Report the operation as canceled and check the waker is called and
    ///   the entry is idle but kept.
    /// - Poll again and check a new `Accept` is in flight.
    #[test]
    fn canceled_accept_is_resubmitted() {
        let Some((_reactor, ops, mut handler)) = setup() else {
            return;
        };
        let mut listener = listener(&ops);
        let (woken, waker) = waker();
        let mut cx = Context::from_waker(&waker);

        assert!(listener.0.poll_accept(&mut cx).is_pending());
        handler.canceled(0);
        assert_eq!(woken.get(), 1);
        assert_eq!(state(&ops), Some(State::Idle));

        assert!(listener.0.poll_accept(&mut cx).is_pending());
        assert_eq!(state(&ops), Some(State::Accept));
    }

    /// STATEMENT: A dropped listener with an operation in flight keeps its
    /// entry until the operation finishes, whatever its outcome.
    ///
    /// HOW:
    /// - For each outcome, poll an accept, optionally move it to the `PollAdd`
    ///   state, then drop the listener and check the entry is kept and marked
    ///   as dropped.
    /// - Deliver a cancellation, a readable poll, an accepted socket, an
    ///   `Interrupted` error or a `WouldBlock` error and check the entry is
    ///   released instead of resubmitted.
    #[test]
    fn dropped_listener_is_released_on_completion() {
        let Some((_reactor, ops, mut handler)) = setup() else {
            return;
        };
        let cases: [(bool, Finish); 5] = [
            (false, |h| h.canceled(0)),
            (true, |h| h.completed(0, 0, Ok(1))),
            (false, |h| h.completed(0, 0, Ok(socket_fd()))),
            (false, |h| {
                h.completed(0, 0, Err(io::ErrorKind::Interrupted.into()));
            }),
            (false, |h| {
                h.completed(0, 0, Err(io::ErrorKind::WouldBlock.into()));
            }),
        ];

        for (poll, finish) in cases {
            let mut listener = listener(&ops);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(listener.0.poll_accept(&mut cx).is_pending());
            if poll {
                handler.completed(0, 0, Err(io::ErrorKind::WouldBlock.into()));
                assert_eq!(state(&ops), Some(State::Poll));
            }

            drop(listener);
            assert!(ops.0.entries.borrow()[0].dropped);
            assert_eq!(handler.in_flight(), 1);

            finish(&mut handler);
            assert!(ops.0.entries.borrow().is_empty());
        }
    }

    /// STATEMENT: Completions for unknown entries are ignored, and after the
    /// reactor is cleaned up a listener reports that the reactor is stopped.
    ///
    /// HOW:
    /// - Deliver a completion and a cancellation for an id without an entry and
    ///   check nothing is created.
    /// - Create a listener, run handler cleanup and check `poll_accept` and
    ///   `local_addr` return the "reactor is stopped" error.
    /// - Drop the listener and check it does not touch the cleared slab.
    #[test]
    fn stopped_reactor_fails_accept() {
        let Some((_reactor, ops, mut handler)) = setup() else {
            return;
        };
        handler.completed(5, 0, Ok(1));
        handler.canceled(5);
        assert!(ops.0.entries.borrow().is_empty());

        let mut listener = listener(&ops);
        assert!(listener.local_addr().is_ok());
        handler.cleanup();

        let mut cx = Context::from_waker(Waker::noop());
        let Poll::Ready(Err(err)) = listener.0.poll_accept(&mut cx) else {
            panic!("expected an error");
        };
        assert_eq!(err.to_string(), "reactor is stopped");
        assert_eq!(
            listener.local_addr().unwrap_err().to_string(),
            "reactor is stopped"
        );
        drop(listener);
        assert!(ops.0.entries.borrow().is_empty());
    }

    /// STATEMENT: An accepted socket with a non-inet peer address is reported
    /// as an error and closed.
    ///
    /// HOW:
    /// - Pass the fd of a new socket and a zeroed address, whose family is
    ///   `AF_UNSPEC`, to `accepted`.
    /// - Check the non-inet address error is returned.
    #[test]
    fn non_inet_peer_address_is_rejected() {
        // SAFETY: all zeros is a valid `sockaddr_storage`
        let addr = (unsafe { mem::zeroed() }, 0);
        let fd = RawFd::try_from(socket_fd()).unwrap();
        let err = accepted(fd, &addr).unwrap_err();
        assert_eq!(err.to_string(), "accepted socket has non-inet address");
    }

    /// STATEMENT: Listeners are rejected when the kernel lacks the accept or
    /// poll opcodes.
    ///
    /// HOW:
    /// - Mark the accept operations as unsupported.
    /// - Create a listener and check an `Unsupported` error is returned.
    #[test]
    fn unsupported_kernel_rejects_listener() {
        let Some((_reactor, ops, _handler)) = setup_with(false) else {
            return;
        };
        let sock = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let err = ops.listener(sock).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
