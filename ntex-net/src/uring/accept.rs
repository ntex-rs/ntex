use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::{cell::RefCell, collections::VecDeque, io, mem, net, rc::Rc};
use std::{task::Context, task::Poll, task::Waker};

use ntex_io_uring::{cqueue::more, opcode, types::Fd};
use ntex_rt::Arbiter;
use slab::Slab;

use super::{Handler, Reactor, ReactorApi};
use crate::{Listener, ListenerOps};

type AcceptResult = io::Result<(net::TcpStream, Option<net::SocketAddr>)>;

/// Accepted connections a listener queues before its multishot accept is
/// canceled, it is armed again once the queue is drained
const MAX_QUEUED: usize = 32;

#[derive(Clone)]
pub(crate) struct AcceptOps(Rc<AcceptOpsInner>);

struct AcceptOpsHandler {
    inner: Rc<AcceptOpsInner>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// Multishot `Accept` is submitted, it posts a completion per connection
    Multi,
    /// `PollAdd` is submitted, the listener reported `EAGAIN`
    Poll,
}

/// Accept state, owned by the slab until no operation is in flight
struct Entry {
    /// The socket stays open until the kernel is done with it
    sock: net::TcpListener,
    state: State,
    /// The multishot accept is canceled because the queue is full
    paused: bool,
    results: VecDeque<AcceptResult>,
    waker: Option<Waker>,
    /// The listener is dropped, the entry is released on completion
    dropped: bool,
}

struct AcceptOpsInner {
    api: ReactorApi,
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
                let ops = Rc::new(AcceptOpsInner {
                    api,
                    entries: RefCell::new(Slab::new()),
                });
                inner = Some(ops.clone());
                Box::new(AcceptOpsHandler { inner: ops })
            });
            AcceptOps(inner.unwrap())
        })
    }

    pub(crate) fn listener(&self, sock: net::TcpListener) -> Listener {
        let id = self.0.entries.borrow_mut().insert(Entry {
            sock,
            state: State::Idle,
            paused: false,
            results: VecDeque::new(),
            waker: None,
            dropped: false,
        });
        let id = u32::try_from(id).expect("Too many listeners");

        Listener::new(TcpListener {
            id,
            ops: self.0.clone(),
        })
    }
}

impl AcceptOpsInner {
    fn submit_accept(&self, id: u32, entry: &mut Entry) {
        let fd = entry.sock.as_raw_fd();
        entry.state = State::Multi;
        // SAFETY: the slab owns the socket until the operation completes
        unsafe {
            self.api.submit(id, move |en| {
                opcode::AcceptMulti::new(en, Fd(fd)).flags(libc::SOCK_CLOEXEC);
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

    fn push_result(&self, id: u32, entry: &mut Entry, res: AcceptResult) {
        if entry.dropped {
            // closes the accepted stream
            return;
        }
        entry.results.push_back(res);
        // the task stopped accepting, leave further connections in the backlog
        if entry.state == State::Multi && !entry.paused && entry.results.len() >= MAX_QUEUED {
            entry.paused = true;
            self.api.cancel(id);
        }
        if let Some(waker) = entry.waker.take() {
            waker.wake();
        }
    }
}

impl ListenerOps for TcpListener {
    fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<AcceptResult> {
        let mut entries = self.ops.entries.borrow_mut();
        let Some(entry) = entries.get_mut(self.id as usize) else {
            return Poll::Ready(Err(io::Error::other("reactor is stopped")));
        };

        if let Some(res) = entry.results.pop_front() {
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
                entry.results.clear();
                if !entry.paused {
                    self.ops.api.cancel(self.id);
                }
            }
        }
    }
}

/// Takes ownership of an accepted socket. Multishot accept does not report the
/// peer address, every completion would overwrite a shared buffer; [`Listener`]
/// queries it only when requested.
fn accepted(fd: RawFd) -> net::TcpStream {
    // SAFETY: the kernel returned a new connected socket owned by us
    unsafe { net::TcpStream::from_raw_fd(fd) }
}

impl Handler for AcceptOpsHandler {
    fn canceled(&mut self, id: usize) {
        log::trace!("accept-op is canceled {id:?}");
        let mut entries = self.inner.entries.borrow_mut();
        if let Some(entry) = entries.get_mut(id) {
            entry.state = State::Idle;
            entry.paused = false;
            if entry.dropped {
                entries.remove(id);
            } else if let Some(waker) = entry.waker.take() {
                waker.wake();
            }
        }
    }

    fn completed(&mut self, id: usize, flags: u32, result: io::Result<usize>) {
        log::trace!("accept-op is completed {id:?} result: {result:?}");
        let mut entries = self.inner.entries.borrow_mut();
        let Some(entry) = entries.get_mut(id) else {
            return;
        };

        // multishot accept stays armed
        if more(flags) {
            if let Ok(fd) = result {
                let stream = accepted(fd as RawFd);
                self.inner.push_result(id as u32, entry, Ok((stream, None)));
            }
            return;
        }

        let state = mem::replace(&mut entry.state, State::Idle);
        entry.paused = false;

        let res = match result {
            Ok(fd) if state == State::Multi => Ok((accepted(fd as RawFd), None)),
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
                    && state != State::Poll
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
            self.inner.push_result(id as u32, entry, res);
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
        let reactor = Reactor::new(8).ok()?;
        let mut inner = None;
        reactor.register(|api| {
            let ops = Rc::new(AcceptOpsInner {
                api,
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
        ops.listener(sock)
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

    const F_MORE: u32 = 1 << 1;

    /// A raw fd of an accepted connection, and the connected client.
    fn connected_fd() -> (usize, net::TcpStream) {
        let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
        let (sock, _) = lst.accept().unwrap();
        (usize::try_from(sock.into_raw_fd()).unwrap(), client)
    }

    /// STATEMENT: A multishot accept queues every accepted connection with its
    /// peer address, and is armed again after its final completion once the
    /// queue is drained.
    ///
    /// HOW:
    /// - Poll an accept and check a multishot accept is in flight.
    /// - Deliver two completions with `F_MORE` and check the waker is called,
    ///   the accept stays in flight and both connections are returned in order
    ///   with the client addresses.
    /// - Deliver a final completion and check the accept is idle, its
    ///   connection is returned and the next poll arms a new multishot accept.
    #[test]
    fn multishot_accept_queues_connections() {
        let Some((_reactor, ops, mut handler)) = setup() else {
            return;
        };
        let mut listener = listener(&ops);
        let (woken, waker) = waker();
        let mut cx = Context::from_waker(&waker);

        assert!(listener.0.poll_accept(&mut cx).is_pending());
        assert_eq!(state(&ops), Some(State::Multi));

        let (fd1, c1) = connected_fd();
        let (fd2, c2) = connected_fd();
        handler.completed(0, F_MORE, Ok(fd1));
        handler.completed(0, F_MORE, Ok(fd2));
        assert_eq!(woken.get(), 1);
        assert_eq!(state(&ops), Some(State::Multi));
        assert_eq!(handler.in_flight(), 1);

        for client in [&c1, &c2] {
            let Poll::Ready(Ok((_, addr))) = listener.poll_accept(&mut cx) else {
                panic!("expected a connection");
            };
            assert_eq!(addr, client.local_addr().unwrap());
        }
        assert!(listener.0.poll_accept(&mut cx).is_pending());
        assert_eq!(state(&ops), Some(State::Multi));

        let (fd3, c3) = connected_fd();
        handler.completed(0, 0, Ok(fd3));
        assert_eq!(state(&ops), Some(State::Idle));
        let Poll::Ready(Ok((_, addr))) = listener.poll_accept(&mut cx) else {
            panic!("expected a connection");
        };
        assert_eq!(addr, c3.local_addr().unwrap());
        assert!(listener.0.poll_accept(&mut cx).is_pending());
        assert_eq!(state(&ops), Some(State::Multi));
    }

    /// STATEMENT: A multishot accept is canceled once `MAX_QUEUED` connections
    /// wait to be returned, and is armed again only after the queue is drained.
    ///
    /// HOW:
    /// - Poll an accept, then deliver `MAX_QUEUED` completions with `F_MORE`
    ///   for unconnected sockets and check the accept is paused.
    /// - Deliver one more completion and check it is still queued.
    /// - Check `Listener::poll_accept` reports an unconnected socket as
    ///   `ConnectionAborted` when it queries the peer address.
    /// - Report the cancellation, then check the remaining connections are
    ///   returned without arming an accept, and the poll after them arms a
    ///   new multishot accept.
    #[test]
    fn multishot_accept_pauses_when_queue_is_full() {
        let Some((_reactor, ops, mut handler)) = setup() else {
            return;
        };
        let mut listener = listener(&ops);
        let mut cx = Context::from_waker(Waker::noop());

        assert!(listener.0.poll_accept(&mut cx).is_pending());
        for _ in 0..MAX_QUEUED {
            handler.completed(0, F_MORE, Ok(socket_fd()));
        }
        assert!(ops.0.entries.borrow()[0].paused);
        handler.completed(0, F_MORE, Ok(socket_fd()));
        assert_eq!(ops.0.entries.borrow()[0].results.len(), MAX_QUEUED + 1);

        let Poll::Ready(Err(err)) = listener.poll_accept(&mut cx) else {
            panic!("expected an error");
        };
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);

        handler.canceled(0);
        assert_eq!(state(&ops), Some(State::Idle));
        assert!(!ops.0.entries.borrow()[0].paused);
        for _ in 0..MAX_QUEUED {
            assert!(listener.0.poll_accept(&mut cx).is_ready());
            assert_eq!(state(&ops), Some(State::Idle));
        }
        assert!(listener.0.poll_accept(&mut cx).is_pending());
        assert_eq!(state(&ops), Some(State::Multi));
    }

    /// STATEMENT: `Listener::poll_accept_stream` returns a multishot-accepted
    /// connection without querying its peer address, so a connection reset
    /// before it was accepted is returned rather than rejected.
    ///
    /// HOW:
    /// - Poll `poll_accept_stream`, then deliver an unconnected socket with
    ///   `F_MORE`.
    /// - Check the next poll returns that socket, and its `peer_addr` fails
    ///   with `NotConnected`.
    #[test]
    fn accept_stream_skips_peer_addr() {
        let Some((_reactor, ops, mut handler)) = setup() else {
            return;
        };
        let mut listener = listener(&ops);
        let mut cx = Context::from_waker(Waker::noop());

        assert!(listener.poll_accept_stream(&mut cx).is_pending());
        handler.completed(0, F_MORE, Ok(socket_fd()));

        let Poll::Ready(Ok(stream)) = listener.poll_accept_stream(&mut cx) else {
            panic!("expected a connection");
        };
        let err = stream.peer_addr().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    }

    /// STATEMENT: A dropped listener closes connections its multishot accept
    /// still delivers, and is released by the final completion.
    ///
    /// HOW:
    /// - Poll an accept, drop the listener and check the entry is kept.
    /// - Deliver a completion with `F_MORE` and check nothing is queued.
    /// - Report the cancellation and check the entry is released.
    #[test]
    fn multishot_accept_of_dropped_listener() {
        let Some((_reactor, ops, mut handler)) = setup() else {
            return;
        };
        let mut listener = listener(&ops);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(listener.0.poll_accept(&mut cx).is_pending());
        drop(listener);

        let (fd, _client) = connected_fd();
        handler.completed(0, F_MORE, Ok(fd));
        assert!(ops.0.entries.borrow()[0].results.is_empty());
        assert_eq!(handler.in_flight(), 1);

        handler.canceled(0);
        assert!(ops.0.entries.borrow().is_empty());
    }

    /// STATEMENT: A multishot accept that fails with `EAGAIN` waits for the
    /// listener to become readable and is armed again, and an interrupted
    /// accept is retried.
    ///
    /// HOW:
    /// - Poll an accept and check a multishot accept is in flight.
    /// - Complete it with `WouldBlock` and check a `PollAdd` is in flight.
    /// - Complete the poll and check a multishot accept is in flight again,
    ///   then complete it with `Interrupted` and check it is resubmitted.
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
        assert_eq!(state(&ops), Some(State::Multi));
        assert_eq!(handler.in_flight(), 1);

        handler.completed(0, 0, Err(io::ErrorKind::WouldBlock.into()));
        assert_eq!(state(&ops), Some(State::Poll));

        handler.completed(0, 0, Ok(1));
        assert_eq!(state(&ops), Some(State::Multi));

        handler.completed(0, 0, Err(io::ErrorKind::Interrupted.into()));
        assert_eq!(state(&ops), Some(State::Multi));

        assert_eq!(woken.get(), 0);
        assert!(ops.0.entries.borrow()[0].results.is_empty());
        assert!(listener.0.poll_accept(&mut cx).is_pending());
    }

    /// STATEMENT: A failed accept wakes the task and its error is returned by
    /// the next accept, which then submits a new operation.
    ///
    /// HOW:
    /// - Poll an accept so a waker is stored and a multishot accept is in
    ///   flight.
    /// - Complete it with `ConnectionAborted` and check the waker is called
    ///   and nothing is in flight.
    /// - Poll again and check the error is returned, then check the following
    ///   poll submits a new multishot accept.
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
        assert_eq!(state(&ops), Some(State::Multi));
    }

    /// STATEMENT: A canceled accept of a live listener wakes the task, and the
    /// next accept submits a new operation.
    ///
    /// HOW:
    /// - Poll an accept so a waker is stored and a multishot accept is in
    ///   flight.
    /// - Report the operation as canceled and check the waker is called and
    ///   the entry is idle but kept.
    /// - Poll again and check a new multishot accept is in flight.
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
        assert_eq!(state(&ops), Some(State::Multi));
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
}
