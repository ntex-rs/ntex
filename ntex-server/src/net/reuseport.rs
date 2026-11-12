//! Per-worker accept for [`LoadBalancing::Reuseport`](crate::LoadBalancing::Reuseport).
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::{fmt, future::Future, io, net, pin::Pin};

use event_listener::{Event, EventListener};
use ntex_util::{future::Stream, time::Sleep, time::sleep};
use parking_lot::Mutex;

use crate::{affinity, metrics::AcceptSlot};

use super::Token;
use super::accept::{ERR_TIMEOUT, connection_error};
use super::socket::{Connection, Stream as IoStream};

/// Listeners of one worker, with their metrics indexes.
pub(crate) type Sockets = Vec<(usize, Token, net::TcpListener)>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    Paused,
    Running,
    Stopped,
}

struct Slot {
    socks: Sockets,
    /// A worker accepts on the slot.
    busy: bool,
}

struct Inner {
    state: State,
    /// `None` if the slot's listeners are closed.
    slots: Vec<Option<Slot>>,
    /// Whether the pause is counted in the listener metrics.
    paused_reported: bool,
}

/// `SO_REUSEPORT` listeners shared by the workers, one slot per worker.
///
/// The listeners are kept open while the server runs, so connections queued
/// for a restarting worker are accepted by its replacement.
pub(crate) struct Reuseport {
    inner: Mutex<Inner>,
    event: Event,
    metrics: OnceLock<Arc<AcceptSlot>>,
}

impl Reuseport {
    /// Creates paused listener slots.
    pub(crate) fn new(slots: Vec<Sockets>) -> Self {
        Reuseport {
            inner: Mutex::new(Inner {
                state: State::Paused,
                slots: slots
                    .into_iter()
                    .map(|socks| Some(Slot { socks, busy: false }))
                    .collect(),
                paused_reported: false,
            }),
            event: Event::new(),
            metrics: OnceLock::new(),
        }
    }

    /// Sets the listener metrics, workers accept only after this call.
    pub(crate) fn set_metrics(&self, metrics: Arc<AcceptSlot>) {
        let _ = self.metrics.set(metrics);
        self.event.notify(usize::MAX);
    }

    pub(crate) fn pause(&self) {
        self.set_state(State::Paused);
    }

    pub(crate) fn resume(&self) {
        self.set_state(State::Running);
    }

    /// Stops accepting and closes the listeners.
    ///
    /// Connections queued on a listener are reset once the worker that
    /// accepts on it drops its listener.
    pub(crate) fn stop(&self) {
        self.set_state(State::Stopped);
    }

    fn set_state(&self, state: State) {
        let mut inner = self.inner.lock();
        if inner.state == State::Stopped {
            return;
        }
        inner.state = state;
        if state == State::Stopped {
            inner.slots.iter_mut().for_each(|slot| *slot = None);
        } else {
            // like the accept loop, only a pause after the start is counted
            let paused = state == State::Paused;
            if paused != inner.paused_reported
                && let Some(metrics) = self.metrics.get()
            {
                metrics.paused(paused);
                inner.paused_reported = paused;
            }
        }
        drop(inner);
        self.event.notify(usize::MAX);
    }

    /// Takes a free slot for the current worker thread.
    pub(crate) fn local(self: &Arc<Self>) -> Option<LocalAccept> {
        let cpu = affinity::current_cpu();
        let (slot, socks) = {
            let mut inner = self.inner.lock();
            if inner.state == State::Stopped {
                return None;
            }
            let Some((idx, slot)) = inner
                .slots
                .iter_mut()
                .enumerate()
                .find_map(|(idx, slot)| slot.as_mut().filter(|s| !s.busy).map(|s| (idx, s)))
            else {
                log::warn!("No free reuseport listeners for the worker");
                return None;
            };
            slot.busy = true;

            let socks = slot
                .socks
                .iter()
                .map(|(idx, token, lst)| {
                    // the option is shared with the clone
                    if let Some(cpu) = cpu
                        && let Err(e) = affinity::set_incoming_cpu(lst, cpu)
                    {
                        log::warn!("Cannot set SO_INCOMING_CPU {cpu} for {lst:?}: {e}");
                    }
                    Ok((*idx, *token, lst.try_clone()?))
                })
                .collect::<io::Result<Vec<_>>>();
            (idx, socks)
        };
        let guard = SlotGuard {
            shared: self.clone(),
            slot,
        };

        let socks = socks.and_then(|socks| {
            socks
                .into_iter()
                .map(|(idx, token, lst)| {
                    lst.set_nonblocking(true)?;
                    Ok((idx, token, ntex_net::tcp_listener(lst)?))
                })
                .collect::<io::Result<Vec<_>>>()
        });
        match socks {
            Ok(socks) => Some(LocalAccept {
                socks,
                next: 0,
                listener: None,
                timeout: None,
                slot: guard,
            }),
            Err(e) => {
                // the kernel sends the connections to the other listeners
                log::error!("Cannot accept connections on the worker: {e}");
                self.inner.lock().slots[slot] = None;
                None
            }
        }
    }
}

impl fmt::Debug for Reuseport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("Reuseport")
            .field("state", &inner.state)
            .field("slots", &inner.slots.len())
            .finish()
    }
}

/// Releases the slot of a worker.
struct SlotGuard {
    shared: Arc<Reuseport>,
    slot: usize,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Some(slot) = &mut self.shared.inner.lock().slots[self.slot] {
            slot.busy = false;
        }
    }
}

/// Connections accepted on the listeners of one worker.
pub(crate) struct LocalAccept {
    socks: Vec<(usize, Token, ntex_net::Listener)>,
    /// Listener polled first, for fairness.
    next: usize,
    /// Waits for server state changes.
    listener: Option<EventListener>,
    /// Back-off after an accept error.
    timeout: Option<Sleep>,
    slot: SlotGuard,
}

impl LocalAccept {
    fn shared(&self) -> &Reuseport {
        &self.slot.shared
    }
}

impl Stream for LocalAccept {
    type Item = Connection;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Connection>> {
        let this = self.get_mut();

        // listen before reading the state, so no change is missed
        loop {
            let shared = &this.slot.shared;
            let listener = this.listener.get_or_insert_with(|| shared.event.listen());
            if Pin::new(listener).poll(cx).is_pending() {
                break;
            }
            this.listener = None;
        }

        let state = this.shared().inner.lock().state;
        match state {
            State::Stopped => {
                this.socks.clear();
                return Poll::Ready(None);
            }
            State::Paused => return Poll::Pending,
            State::Running => {}
        }
        let Some(metrics) = this.shared().metrics.get().cloned() else {
            return Poll::Pending;
        };

        if let Some(ref timeout) = this.timeout {
            if timeout.poll_elapsed(cx).is_pending() {
                return Poll::Pending;
            }
            this.timeout = None;
        }

        let num = this.socks.len();
        for n in 0..num {
            let pos = (this.next + n) % num;
            let (idx, token, ref mut lst) = this.socks[pos];
            loop {
                match lst.poll_accept(cx) {
                    Poll::Ready(Ok((stream, _))) => {
                        metrics.accepted(idx);
                        this.next = pos + 1;
                        return Poll::Ready(Some(Connection {
                            io: IoStream::Tcp(stream),
                            token,
                        }));
                    }
                    Poll::Ready(Err(ref e)) if connection_error(e) => {
                        metrics.connection_error(idx);
                    }
                    Poll::Ready(Err(e)) => {
                        metrics.resource_error(idx);
                        log::error!("Error accepting connection: {e}");
                        let timeout = sleep(ERR_TIMEOUT);
                        let _ = timeout.poll_elapsed(cx);
                        this.timeout = Some(timeout);
                        return Poll::Pending;
                    }
                    Poll::Pending => break,
                }
            }
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::{future::stream_recv, time::Millis, time::timeout};

    use super::*;
    use crate::metrics::Metrics;

    fn listener() -> (net::TcpListener, net::SocketAddr) {
        let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
        lst.set_nonblocking(true).unwrap();
        let addr = lst.local_addr().unwrap();
        (lst, addr)
    }

    async fn next(acc: &mut LocalAccept) -> Option<Option<Connection>> {
        timeout(Millis(100), stream_recv(acc)).await.ok()
    }

    /// STATEMENT: A worker takes a free slot, the slot is freed when its
    /// `LocalAccept` is dropped, and a stop ends accepting.
    ///
    /// HOW:
    /// - Create one slot and take it with `local`.
    /// - Check a second `local` call finds no free slot.
    /// - Drop the `LocalAccept` and check the slot can be taken again.
    /// - Stop and check the stream ends and `local` returns `None`.
    #[ntex::test]
    async fn slots() {
        if !ntex_net::is_tcp_listener_supported() {
            return;
        }
        let (lst, _) = listener();
        let rp = Arc::new(Reuseport::new(vec![vec![(0, Token(0), lst)]]));
        assert!(format!("{rp:?}").contains("Paused"));

        let acc = rp.local().unwrap();
        assert!(rp.local().is_none());
        drop(acc);
        let mut acc = rp.local().unwrap();

        rp.stop();
        assert!(matches!(next(&mut acc).await, Some(None)));
        assert!(rp.local().is_none());
        // a stopped server is not resumed
        rp.resume();
        assert!(rp.local().is_none());
    }

    /// STATEMENT: Connections are accepted only while the server runs and
    /// after the listener metrics are set, and pauses are counted.
    ///
    /// HOW:
    /// - Resume the slots before the metrics are set, connect, and check no
    ///   connection is accepted.
    /// - Set the metrics and check the connection is accepted and counted.
    /// - Pause, connect, and check no connection is accepted while the pause
    ///   is counted.
    /// - Resume and check the queued connection is accepted.
    #[ntex::test]
    async fn accept_requires_metrics_and_running() {
        if !ntex_net::is_tcp_listener_supported() {
            return;
        }
        let (lst, addr) = listener();
        let rp = Arc::new(Reuseport::new(vec![vec![(0, Token(0), lst)]]));
        let mut acc = rp.local().unwrap();

        rp.resume();
        let _c1 = net::TcpStream::connect(addr).unwrap();
        assert!(next(&mut acc).await.is_none());

        let metrics = Metrics::new(1);
        metrics.listeners([(String::new(), addr.to_string())]);
        rp.set_metrics(metrics.accept_slot().unwrap());
        assert!(matches!(next(&mut acc).await, Some(Some(_))));
        assert_eq!(metrics.snapshot().listeners[0].accepted, 1);

        rp.pause();
        assert_eq!(metrics.snapshot().accept_pauses, 1);
        let _c2 = net::TcpStream::connect(addr).unwrap();
        assert!(next(&mut acc).await.is_none());

        rp.resume();
        assert!(matches!(next(&mut acc).await, Some(Some(_))));
        assert_eq!(metrics.snapshot().listeners[0].accepted, 2);
    }
}
