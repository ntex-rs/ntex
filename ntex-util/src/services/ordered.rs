//! Service that runs requests concurrently but enters a second service in ticket order.
//!
//! [`Ordered`] calls service `A` for every request concurrently, then passes each
//! result to service `B` strictly in ticket order. Each caller receives its own
//! `B` response. This keeps an ordered side effect in `B`, such as appending to a
//! log or queue, in request order while slow `A` processing (authorization,
//! payload reads) still overlaps.
//!
//! Tickets come from a [`Sequencer`]:
//!
//! - [`Arrival`] issues tickets in the order calls start, optionally only for
//!   requests selected by a filter;
//! - [`tickets()`] takes tickets assigned by an external system, for example a
//!   protocol codec that numbers packets in decode order.
//!
//! # Example
//!
//! ```
//! use ntex_service::{Pipeline, fn_service};
//! use ntex_util::services::ordered::Ordered;
//!
//! # #[ntex::main]
//! # async fn main() {
//! let svc = Ordered::new(
//!     fn_service(async |req: u32| Ok::<_, ()>(req * 2)),
//!     fn_service(async |val: u32| Ok::<_, ()>(val.to_string())),
//! );
//! let pipeline = Pipeline::new((), svc);
//! assert_eq!(pipeline.call(21).await, Ok("42".to_string()));
//! # }
//! ```
use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{collections::VecDeque, fmt, future::Future, future::poll_fn, pin::pin, rc::Rc};

use ntex_service::{BindCtx, Ctx, ReadyBinding, Service, ServiceFactory};

use crate::task::LocalWaker;

/// Position of a request in an ordered sequence.
///
/// Dropping a ticket that holds the turn passes the turn to the next ticket;
/// dropping a ticket before its turn skips its position.
pub trait Turn {
    /// Polls until this ticket holds the turn.
    fn poll_turn(&self, cx: &mut Context<'_>) -> Poll<()>;
}

/// Source of tickets for [`Ordered`].
pub trait Sequencer<Req> {
    /// Ticket type.
    type Ticket: Turn;

    /// Returns the ticket of a request, `None` for a request that is not ordered.
    ///
    /// Called synchronously when [`Ordered`] starts handling the request.
    fn ticket(&self, req: &mut Req) -> Option<Self::Ticket>;
}

/// Position in a sequence issued by [`Tickets`].
///
/// The oldest outstanding ticket holds the turn. Passing or dropping it hands
/// the turn to the next ticket; a ticket dropped before its turn is skipped.
///
/// Holding a ticket blocks the turn of every later ticket of the sequence,
/// pass or drop it once its ordered side effect is done.
pub struct Ticket {
    seq: u64,
    turns: Rc<Turns>,
}

impl Ticket {
    #[inline]
    /// Position in the sequence, starting from zero.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    #[inline]
    /// Returns `true` if this ticket holds the turn.
    pub fn is_turn(&self) -> bool {
        self.turns.turn.get() == self.seq
    }

    /// Polls until this ticket holds the turn.
    pub fn poll_turn(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.turns.poll_turn(self.seq, cx)
    }

    /// Waits until this ticket holds the turn.
    pub async fn turn(&self) {
        poll_fn(|cx| self.poll_turn(cx)).await;
    }

    /// Releases the ticket, same as drop.
    ///
    /// If the ticket holds the turn, the turn passes to the next outstanding ticket.
    pub fn pass(self) {}
}

impl Turn for Ticket {
    fn poll_turn(&self, cx: &mut Context<'_>) -> Poll<()> {
        Ticket::poll_turn(self, cx)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.turns.release(self.seq);
    }
}

impl PartialEq for Ticket {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq && Rc::ptr_eq(&self.turns, &other.turns)
    }
}

impl Eq for Ticket {}

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ticket").field("seq", &self.seq).finish()
    }
}

/// Issuer of a [`Ticket`] sequence.
#[derive(Default)]
pub struct Tickets(Rc<Turns>);

impl Tickets {
    /// Creates a new sequence, the first ticket is `0`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues the next ticket.
    pub fn issue(&self) -> Ticket {
        let turns = &self.0;
        let seq = turns.next.get();
        turns.next.set(seq.wrapping_add(1));
        turns.slots.borrow_mut().push_back(Slot::default());
        Ticket {
            seq,
            turns: turns.clone(),
        }
    }
}

impl fmt::Debug for Tickets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tickets")
            .field("next", &self.0.next.get())
            .field("turn", &self.0.turn.get())
            .finish()
    }
}

/// Turn state shared by an issuer and its outstanding tickets.
#[derive(Default)]
struct Turns {
    /// Sequence of the next issued ticket.
    next: Cell<u64>,
    /// Sequence holding the turn, matches the front of `slots`.
    turn: Cell<u64>,
    /// Outstanding tickets from `turn` to `next`, indexed by `seq - turn`.
    slots: RefCell<VecDeque<Slot>>,
}

#[derive(Default)]
struct Slot {
    /// Ticket is dropped, its turn is skipped.
    released: bool,
    /// Task waiting for the turn.
    waker: LocalWaker,
}

impl Turns {
    #[allow(clippy::cast_possible_truncation)]
    fn index(&self, seq: u64) -> usize {
        // outstanding tickets fit in memory, so the distance fits `usize`
        seq.wrapping_sub(self.turn.get()) as usize
    }

    fn poll_turn(&self, seq: u64, cx: &Context<'_>) -> Poll<()> {
        if seq == self.turn.get() {
            Poll::Ready(())
        } else {
            let idx = self.index(seq);
            self.slots.borrow()[idx].waker.register(cx.waker());
            Poll::Pending
        }
    }

    fn release(&self, seq: u64) {
        let waker = {
            let idx = self.index(seq);
            let mut slots = self.slots.borrow_mut();
            let mut turn = self.turn.get();
            slots[idx].released = true;
            while slots.front().is_some_and(|s| s.released) {
                slots.pop_front();
                turn = turn.wrapping_add(1);
            }
            self.turn.set(turn);
            slots.front().and_then(|s| s.waker.take())
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Sequencer that issues tickets in the order calls start.
///
/// Every service created by [`OrderedFactory`] and every clone starts its own
/// sequence. Without a filter every request is ordered; with a filter, requests
/// for which it returns `false` are not ordered.
pub struct Arrival<F = ()> {
    tickets: Tickets,
    filter: F,
}

impl Arrival {
    /// Creates a sequencer that orders every request.
    pub fn new() -> Self {
        Self {
            tickets: Tickets::new(),
            filter: (),
        }
    }
}

impl Default for Arrival {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> Arrival<F> {
    /// Orders only requests for which `filter` returns `true`.
    pub fn filter<Req, F2>(self, filter: F2) -> Arrival<F2>
    where
        F2: Fn(&Req) -> bool,
    {
        Arrival {
            filter,
            tickets: self.tickets,
        }
    }
}

impl<F: Clone> Clone for Arrival<F> {
    fn clone(&self) -> Self {
        Self {
            tickets: Tickets::new(),
            filter: self.filter.clone(),
        }
    }
}

impl<F> fmt::Debug for Arrival<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Arrival")
            .field("tickets", &self.tickets)
            .finish()
    }
}

impl<Req> Sequencer<Req> for Arrival {
    type Ticket = Ticket;

    fn ticket(&self, _: &mut Req) -> Option<Ticket> {
        Some(self.tickets.issue())
    }
}

impl<Req, F> Sequencer<Req> for Arrival<F>
where
    F: Fn(&Req) -> bool,
{
    type Ticket = Ticket;

    fn ticket(&self, req: &mut Req) -> Option<Ticket> {
        (self.filter)(req).then(|| self.tickets.issue())
    }
}

/// Sequencer that takes tickets from requests, see [`tickets()`].
#[derive(Clone)]
pub struct External<F>(F);

/// Creates a sequencer that takes tickets assigned by an external system.
///
/// `f` extracts the ticket from a request, `None` means the request is not ordered.
/// Requests may enter [`Ordered`] in any order, they enter `B` in ticket order.
///
/// ```
/// use ntex_util::services::ordered::{Ordered, Ticket, tickets};
/// # let a = ntex_service::fn_service(async |req: (u32, Option<Ticket>)| Ok::<_, ()>(req.0));
/// # let b = ntex_service::fn_service(async |val: u32| Ok::<_, ()>(val));
///
/// let svc = Ordered::new(a, b).sequencer(tickets(|req: &mut (u32, Option<Ticket>)| req.1.take()));
/// ```
pub fn tickets<F, Req, T>(f: F) -> External<F>
where
    F: Fn(&mut Req) -> Option<T>,
    T: Turn,
{
    External(f)
}

impl<F> fmt::Debug for External<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("External").finish()
    }
}

impl<Req, F, T> Sequencer<Req> for External<F>
where
    F: Fn(&mut Req) -> Option<T>,
    T: Turn,
{
    type Ticket = T;

    fn ticket(&self, req: &mut Req) -> Option<T> {
        (self.0)(req)
    }
}

/// Runs `A` concurrently and enters `B` in ticket order.
///
/// For each request, [`Ordered`] takes a ticket from its [`Sequencer`], calls `A`,
/// waits for the ticket's turn, waits for `B` readiness and enters `B::call`. The
/// turn passes to the next ticket as soon as `B::call` is entered, so `B` calls run
/// concurrently and each caller awaits its own `B` result. Requests without a
/// ticket call `A` and `B` without waiting for a turn.
///
/// Errors from `A` and from `B` readiness are returned immediately and pass the
/// turn. Dropping the call future at any stage passes or skips its turn.
///
/// Readiness requires both services to be ready, so `B` backpressure applies to
/// new requests. `B` has its own readiness cache ([`ReadyBinding`]).
///
/// # Contracts
///
/// - `B::call` must perform its ordered side effect before its first `.await`;
///   the turn passes once `B::call` is first polled.
/// - `B` must notify readiness changes (see [`ntex_service::ReadyNotifier`]).
/// - With [`Arrival`], the ticket is taken on the first poll of the call future,
///   so callers must start calls in arrival order: layers above `Ordered` must not
///   await before calling it. External tickets ([`tickets()`]) remove this
///   requirement.
/// - A held ticket blocks later turns, external tickets must be passed to
///   `Ordered` or dropped.
pub struct Ordered<A, B, S = Arrival> {
    a: A,
    b: ReadyBinding<B>,
    seq: S,
}

impl<A, B> Ordered<A, B> {
    /// Creates a service that orders every request by call start.
    pub fn new(a: A, b: B) -> Self {
        Self {
            a,
            b: ReadyBinding::new(b),
            seq: Arrival::new(),
        }
    }
}

impl<A, B, F> Ordered<A, B, Arrival<F>> {
    /// Orders only requests for which `filter` returns `true`.
    pub fn filter<Req, F2>(self, filter: F2) -> Ordered<A, B, Arrival<F2>>
    where
        F2: Fn(&Req) -> bool,
    {
        Ordered {
            a: self.a,
            b: self.b,
            seq: self.seq.filter(filter),
        }
    }
}

impl<A, B, S> Ordered<A, B, S> {
    /// Replaces the ticket source.
    pub fn sequencer<S2>(self, seq: S2) -> Ordered<A, B, S2> {
        Ordered {
            a: self.a,
            b: self.b,
            seq,
        }
    }
}

impl<A: Clone, B: Clone, S: Clone> Clone for Ordered<A, B, S> {
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
            seq: self.seq.clone(),
        }
    }
}

impl<A: fmt::Debug, B: fmt::Debug, S: fmt::Debug> fmt::Debug for Ordered<A, B, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ordered")
            .field("a", &self.a)
            .field("b", self.b.get_ref())
            .field("seq", &self.seq)
            .finish()
    }
}

impl<A, B, S, St, Req> Service<St, Req> for Ordered<A, B, S>
where
    A: Service<St, Req>,
    B: Service<St, A::Res, Error = A::Error>,
    S: Sequencer<Req>,
{
    type Res = B::Res;
    type Error = A::Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.bind(&self.a);
        ctx.bind(&self.b);
    }

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), A::Error> {
        let mut a = pin!(ctx.ready(&self.a));
        let mut b = pin!(ctx.ready(&self.b));
        let (mut a_ready, mut b_ready) = (false, false);
        poll_fn(|cx| {
            if !a_ready && let Poll::Ready(res) = a.as_mut().poll(cx) {
                res?;
                a_ready = true;
            }
            if !b_ready && let Poll::Ready(res) = b.as_mut().poll(cx) {
                res?;
                b_ready = true;
            }
            if a_ready && b_ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
        .await
    }

    async fn call(&self, mut req: Req, ctx: Ctx<'_, Self, St>) -> Result<B::Res, A::Error> {
        // taken before any await, drop passes or skips the turn
        let Some(ticket) = self.seq.ticket(&mut req) else {
            let res = ctx.call(&self.a, req).await?;
            return ctx.call(&self.b, res).await;
        };

        let res = ctx.call(&self.a, req).await?;
        poll_fn(|cx| ticket.poll_turn(cx)).await;

        // `ctx.call` enters `B::call` in the same poll as the successful
        // readiness check, `B`'s binding cache is valid until then
        let entered = Cell::new(false);
        let mut fut = pin!(async {
            ctx.ready(&self.b).await?;
            entered.set(true);
            ctx.call(&self.b, res).await
        });
        let mut ticket = Some(ticket);
        poll_fn(|cx| {
            let res = fut.as_mut().poll(cx);
            if entered.get() {
                ticket = None;
            }
            res
        })
        .await
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        crate::future::join(ctx.shutdown(&self.a), ctx.shutdown(&self.b)).await;
    }
}

/// Factory for [`Ordered`] services.
///
/// Creates `A` and then `B` with the same state. Each created service clones
/// the sequencer, an [`Arrival`] sequencer starts a new sequence per service.
pub struct OrderedFactory<A, B, S = Arrival> {
    a: A,
    b: B,
    seq: S,
}

impl<A, B> OrderedFactory<A, B> {
    /// Creates a factory that orders every request by call start.
    pub fn new(a: A, b: B) -> Self {
        Self {
            a,
            b,
            seq: Arrival::new(),
        }
    }
}

impl<A, B, F> OrderedFactory<A, B, Arrival<F>> {
    /// Orders only requests for which `filter` returns `true`.
    pub fn filter<Req, F2>(self, filter: F2) -> OrderedFactory<A, B, Arrival<F2>>
    where
        F2: Fn(&Req) -> bool,
    {
        OrderedFactory {
            a: self.a,
            b: self.b,
            seq: self.seq.filter(filter),
        }
    }
}

impl<A, B, S> OrderedFactory<A, B, S> {
    /// Replaces the ticket source.
    pub fn sequencer<S2>(self, seq: S2) -> OrderedFactory<A, B, S2> {
        OrderedFactory {
            a: self.a,
            b: self.b,
            seq,
        }
    }
}

impl<A: Clone, B: Clone, S: Clone> Clone for OrderedFactory<A, B, S> {
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
            seq: self.seq.clone(),
        }
    }
}

impl<A: fmt::Debug, B: fmt::Debug, S: fmt::Debug> fmt::Debug for OrderedFactory<A, B, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrderedFactory")
            .field("a", &self.a)
            .field("b", &self.b)
            .field("seq", &self.seq)
            .finish()
    }
}

impl<A, B, S, St, Req> ServiceFactory<St, Req> for OrderedFactory<A, B, S>
where
    A: ServiceFactory<St, Req>,
    B: ServiceFactory<St, A::Res, Error = A::Error, InitError = A::InitError>,
    S: Sequencer<Req> + Clone,
{
    type Res = B::Res;
    type Error = A::Error;
    type Service = Ordered<A::Service, B::Service, S>;
    type InitError = A::InitError;

    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        let a = self.a.create(st).await?;
        let b = self.b.create(st).await?;
        Ok(Ordered {
            a,
            b: ReadyBinding::new(b),
            seq: self.seq.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::{convert::Infallible, sync::Arc, task::Wake, task::Waker};

    use ntex::rt::{JoinHandle, spawn};
    use ntex_service::{Pipeline, ReadyNotifier, fn_factory, fn_service, service};

    use super::*;
    use crate::services::inflight::InFlightService;
    use crate::{time::Millis, time::sleep, time::timeout};

    fn issue(tickets: &Tickets, n: usize) -> Vec<Ticket> {
        (0..n).map(|_| tickets.issue()).collect()
    }

    /// STATEMENT: Tickets are issued in sequence and the oldest one holds the turn.
    /// HOW: Issue two tickets, check sequence, turn, `Debug` and equality.
    #[test]
    fn issue_in_sequence() {
        let tickets = Tickets::new();
        let t0 = tickets.issue();
        let t1 = tickets.issue();
        assert_eq!((t0.seq(), t1.seq()), (0, 1));
        assert!(t0.is_turn());
        assert!(!t1.is_turn());
        assert_eq!(format!("{t1:?}"), "Ticket { seq: 1 }");
        assert_ne!(t0, t1);
        assert_eq!(t0, t0);

        // another issuer has an independent sequence
        let other = Tickets::new();
        let o0 = other.issue();
        assert_eq!(o0.seq(), 0);
        assert!(o0.is_turn());
        assert_ne!(o0, t0);
    }

    /// STATEMENT: Passing the turn skips tickets dropped before their turn.
    /// HOW: Drop middle tickets out of order, pass the first, the last holds the turn.
    #[test]
    fn pass_and_skip() {
        let tickets = Tickets::new();
        let mut t = issue(&tickets, 5);
        let t4 = t.pop().unwrap();
        let t3 = t.pop().unwrap();
        let t2 = t.pop().unwrap();
        let t1 = t.pop().unwrap();
        let t0 = t.pop().unwrap();

        drop(t1);
        drop(t3);
        drop(t2);
        assert!(t0.is_turn());
        assert!(!t4.is_turn());

        t0.pass();
        assert!(t4.is_turn());
        drop(t4);

        let t5 = tickets.issue();
        assert_eq!(t5.seq(), 5);
        assert!(t5.is_turn());
    }

    struct Flag(AtomicBool);

    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// STATEMENT: The next turn owner is woken only when it gets the turn.
    /// HOW: Wait for the turn of the third ticket, drop the second, pass the first.
    #[test]
    fn wake_next_owner() {
        let tickets = Tickets::new();
        let mut t = issue(&tickets, 3);
        let t2 = t.pop().unwrap();
        let t1 = t.pop().unwrap();
        let t0 = t.pop().unwrap();

        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(flag.clone());
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(t2.turn());
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        drop(t1);
        assert!(!flag.0.load(Ordering::SeqCst), "not woken before its turn");
        t0.pass();
        assert!(flag.0.load(Ordering::SeqCst));
        assert!(fut.as_mut().poll(&mut cx).is_ready());
    }

    #[derive(Debug)]
    struct Msg {
        id: u32,
        delay: u32,
        ordered: bool,
        fail: bool,
        ticket: Option<Ticket>,
    }

    impl Msg {
        fn new(id: u32, delay: u32) -> Self {
            Self {
                id,
                delay,
                ordered: true,
                fail: false,
                ticket: None,
            }
        }

        fn unordered(self) -> Self {
            Self {
                ordered: false,
                ..self
            }
        }

        fn failed(self) -> Self {
            Self { fail: true, ..self }
        }

        fn ticket(self, ticket: Ticket) -> Self {
            Self {
                ticket: Some(ticket),
                ..self
            }
        }
    }

    async fn first(msg: Msg) -> Result<u32, u32> {
        if msg.delay > 0 {
            sleep(Millis(msg.delay)).await;
        }
        if msg.fail { Err(msg.id) } else { Ok(msg.id) }
    }

    #[derive(Clone, Default)]
    struct Sink(Rc<SinkInner>);

    #[derive(Default)]
    struct SinkInner {
        entered: RefCell<Vec<u32>>,
        not_ready: Cell<bool>,
        fail_ready: Cell<bool>,
        delay: Cell<u32>,
        waker: LocalWaker,
        changed: ReadyNotifier,
    }

    impl Sink {
        fn set_ready(&self, ready: bool) {
            self.0.not_ready.set(!ready);
            self.0.changed.notify();
            self.0.waker.wake();
        }

        fn fail_ready(&self) {
            self.0.fail_ready.set(true);
            self.0.changed.notify();
        }

        fn entered(&self) -> Vec<u32> {
            self.0.entered.borrow().clone()
        }
    }

    impl Service<(), u32> for Sink {
        type Res = u32;
        type Error = u32;

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.subscribe(&self.0.changed);
        }

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), u32> {
            poll_fn(|cx| {
                if self.0.fail_ready.replace(false) {
                    return Poll::Ready(Err(0));
                }
                self.0.waker.register(cx.waker());
                if self.0.not_ready.get() {
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            })
            .await
        }

        async fn call(&self, id: u32, _: Ctx<'_, Self>) -> Result<u32, u32> {
            self.0.entered.borrow_mut().push(id);
            let delay = self.0.delay.get();
            if delay > 0 {
                sleep(Millis(delay)).await;
            }
            Ok(id * 10)
        }
    }

    type Svc = Pipeline<Msg, u32, u32>;

    fn ordered(sink: &Sink) -> Svc {
        Pipeline::new(
            (),
            Ordered::new(fn_service(first), sink.clone()).filter(|m: &Msg| m.ordered),
        )
    }

    fn with_inflight(sink: &Sink, limit: usize) -> Svc {
        let svc = Ordered::new(fn_service(first), sink.clone());
        Pipeline::new((), InFlightService::new(limit, svc))
    }

    /// Polls the future once inline (as ntex dispatcher does), then spawns it.
    async fn start<F>(fut: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let mut fut = Box::pin(fut);
        let first = poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx))).await;
        spawn(async move {
            match first {
                Poll::Ready(res) => res,
                Poll::Pending => fut.await,
            }
        })
    }

    /// Dispatcher-like loop: waits for readiness, then calls the service.
    async fn dispatch(
        pl: &Svc,
        msgs: impl IntoIterator<Item = Msg>,
    ) -> Vec<JoinHandle<Result<u32, u32>>> {
        let mut handles = Vec::new();
        for msg in msgs {
            timeout(Millis(1000), pl.ready())
                .await
                .expect("dispatcher readiness stalled")
                .unwrap();
            handles.push(start(pl.call_static(msg)).await);
        }
        handles
    }

    async fn results<T>(handles: Vec<JoinHandle<T>>) -> Vec<T> {
        let mut res = Vec::new();
        for h in handles {
            res.push(
                timeout(Millis(1000), h)
                    .await
                    .expect("call stalled")
                    .unwrap(),
            );
        }
        res
    }

    /// STATEMENT: `B` is entered in arrival order and each caller gets its own result.
    /// HOW: Later requests finish `A` first; check entry order and per-caller results.
    #[ntex::test]
    async fn enters_in_arrival_order() {
        let sink = Sink::default();
        let pl = ordered(&sink);

        let msgs = [
            Msg::new(1, 30),
            Msg::new(2, 20),
            Msg::new(3, 10),
            Msg::new(4, 0),
        ];
        let handles = dispatch(&pl, msgs).await;
        assert_eq!(results(handles).await, vec![Ok(10), Ok(20), Ok(30), Ok(40)]);
        assert_eq!(sink.entered(), vec![1, 2, 3, 4]);
    }

    /// STATEMENT: Requests waiting for `B` readiness enter it in arrival order.
    /// HOW: Make `B` not ready while all requests wait, then make it ready.
    #[ntex::test]
    async fn b_not_ready_keeps_order() {
        let sink = Sink::default();
        let pl = ordered(&sink);

        let handles = dispatch(&pl, [Msg::new(1, 20), Msg::new(2, 10), Msg::new(3, 0)]).await;
        sink.set_ready(false);
        sleep(Millis(40)).await;
        assert_eq!(sink.entered(), Vec::<u32>::new());

        sink.set_ready(true);
        assert_eq!(results(handles).await, vec![Ok(10), Ok(20), Ok(30)]);
        assert_eq!(sink.entered(), vec![1, 2, 3]);
    }

    /// STATEMENT: Order holds when `B` readiness changes repeatedly during turns.
    /// HOW: Toggle `B` readiness through `ReadyNotifier` while requests wait for turns.
    #[ntex::test]
    async fn b_readiness_toggles() {
        let sink = Sink::default();
        let pl = ordered(&sink);

        let toggle = sink.clone();
        spawn(async move {
            for i in 0..10 {
                sleep(Millis(4)).await;
                toggle.set_ready(i % 2 == 1);
            }
        });

        let msgs = (1..=6).map(|id| Msg::new(id, 30 - id * 5));
        let handles = dispatch(&pl, msgs).await;
        let expected: Vec<_> = (1..=6).map(|id| Ok(id * 10)).collect();
        assert_eq!(results(handles).await, expected);
        assert_eq!(sink.entered(), vec![1, 2, 3, 4, 5, 6]);
    }

    /// STATEMENT: An `A` error is returned immediately and does not block later turns.
    /// HOW: Fail the turn owner and a waiting request.
    #[ntex::test]
    async fn a_error() {
        let sink = Sink::default();
        let pl = ordered(&sink);

        let msgs = [
            Msg::new(1, 30).failed(),
            Msg::new(2, 20),
            Msg::new(3, 0).failed(),
            Msg::new(4, 10),
        ];
        let handles = dispatch(&pl, msgs).await;
        assert_eq!(results(handles).await, vec![Err(1), Ok(20), Err(3), Ok(40)]);
        assert_eq!(sink.entered(), vec![2, 4]);
    }

    /// STATEMENT: A `B` readiness error is returned to the turn owner and passes the turn.
    /// HOW: Fail `B` readiness once while requests wait for turns.
    #[ntex::test]
    async fn b_ready_error() {
        let sink = Sink::default();
        let pl = ordered(&sink);

        let handles = dispatch(&pl, [Msg::new(1, 10), Msg::new(2, 0), Msg::new(3, 0)]).await;
        sink.fail_ready();
        assert_eq!(results(handles).await, vec![Err(0), Ok(20), Ok(30)]);
        assert_eq!(sink.entered(), vec![2, 3]);
    }

    /// STATEMENT: Cancelling a request before its turn skips it.
    /// HOW: Time out the second request while the first is in `A`.
    #[ntex::test]
    async fn cancel_before_turn() {
        let sink = Sink::default();
        let pl = ordered(&sink);

        let h1 = start(pl.call_static(Msg::new(1, 30))).await;
        let h2 = start(timeout(Millis(15), pl.call_static(Msg::new(2, 0)))).await;
        let h3 = start(pl.call_static(Msg::new(3, 0))).await;

        assert_eq!(results(vec![h1, h3]).await, vec![Ok(10), Ok(30)]);
        assert!(results(vec![h2]).await[0].is_err());
        assert_eq!(sink.entered(), vec![1, 3]);
    }

    /// STATEMENT: Cancelling the turn owner while it waits for `B` passes the turn.
    /// HOW: Time out the first request while `B` is not ready.
    #[ntex::test]
    async fn cancel_turn_owner() {
        let sink = Sink::default();
        let pl = ordered(&sink);

        sink.set_ready(false);
        let h1 = start(timeout(Millis(20), pl.call_static(Msg::new(1, 0)))).await;
        let h2 = start(pl.call_static(Msg::new(2, 0))).await;
        sleep(Millis(40)).await;
        assert!(results(vec![h1]).await[0].is_err());
        assert_eq!(sink.entered(), Vec::<u32>::new());

        sink.set_ready(true);
        assert_eq!(results(vec![h2]).await, vec![Ok(20)]);
        assert_eq!(sink.entered(), vec![2]);
    }

    /// STATEMENT: Cancelling a request inside `B` does not affect later turns.
    /// HOW: Time out the first request while `B::call` sleeps.
    #[ntex::test]
    async fn cancel_in_b() {
        let sink = Sink::default();
        sink.0.delay.set(30);
        let pl = ordered(&sink);

        let h1 = start(timeout(Millis(10), pl.call_static(Msg::new(1, 0)))).await;
        let h2 = start(pl.call_static(Msg::new(2, 20))).await;
        let h3 = start(pl.call_static(Msg::new(3, 0))).await;

        assert!(results(vec![h1]).await[0].is_err());
        assert_eq!(results(vec![h2, h3]).await, vec![Ok(20), Ok(30)]);
        assert_eq!(sink.entered(), vec![1, 2, 3]);
    }

    /// STATEMENT: Requests rejected by the filter bypass turns.
    /// HOW: An unordered request enters `B` while an earlier ordered one is in `A`.
    #[ntex::test]
    async fn unordered_bypass() {
        let sink = Sink::default();
        let pl = ordered(&sink);

        let msgs = [Msg::new(1, 30), Msg::new(2, 0).unordered(), Msg::new(3, 0)];
        let handles = dispatch(&pl, msgs).await;
        assert_eq!(results(handles).await, vec![Ok(10), Ok(20), Ok(30)]);
        assert_eq!(sink.entered(), vec![2, 1, 3]);
    }

    /// STATEMENT: A full in-flight limit above `Ordered` does not stall turns.
    /// HOW: Limits 1 and 2, with and without a `B` readiness stall.
    #[ntex::test]
    async fn inflight_limit() {
        for limit in [1, 2] {
            for b_stall in [false, true] {
                let sink = Sink::default();
                let pl = with_inflight(&sink, limit);

                if b_stall {
                    let sink = sink.clone();
                    spawn(async move {
                        sleep(Millis(5)).await;
                        sink.set_ready(false);
                        sleep(Millis(60)).await;
                        sink.set_ready(true);
                    });
                }

                let msgs = [
                    Msg::new(1, 30),
                    Msg::new(2, 20),
                    Msg::new(3, 10),
                    Msg::new(4, 0),
                ];
                let handles = dispatch(&pl, msgs).await;
                assert_eq!(
                    results(handles).await,
                    vec![Ok(10), Ok(20), Ok(30), Ok(40)],
                    "limit {limit}, stall {b_stall}"
                );
                assert_eq!(
                    sink.entered(),
                    vec![1, 2, 3, 4],
                    "limit {limit}, stall {b_stall}"
                );
            }
        }
    }

    /// STATEMENT: In-call readiness checks in `A` do not stall turns.
    /// HOW: `A` is an `and_then` chain under an in-flight limit of 2.
    #[ntex::test]
    async fn chained_a_with_inflight_limit() {
        let sink = Sink::default();
        let a = service(fn_service(first)).and_then(fn_service(async |id: u32| Ok::<_, u32>(id)));
        let pl = Pipeline::new((), InFlightService::new(2, Ordered::new(a, sink.clone())));

        let msgs = [
            Msg::new(1, 30),
            Msg::new(2, 10),
            Msg::new(3, 0),
            Msg::new(4, 0),
        ];
        let handles = dispatch(&pl, msgs).await;
        assert_eq!(results(handles).await, vec![Ok(10), Ok(20), Ok(30), Ok(40)]);
        assert_eq!(sink.entered(), vec![1, 2, 3, 4]);
    }

    /// STATEMENT: External tickets define the order, not the call order.
    /// HOW: Call in reverse ticket order with a ticket dropped before its call.
    #[ntex::test]
    async fn external_tickets() {
        let sink = Sink::default();
        let svc = Ordered::new(fn_service(first), sink.clone())
            .sequencer(tickets(|m: &mut Msg| m.ticket.take()));
        let pl = Pipeline::new((), svc);

        let tickets = Tickets::new();
        let mut t = issue(&tickets, 4);
        let t3 = t.pop().unwrap();
        let t2 = t.pop().unwrap();
        drop(t.pop());
        let t0 = t.pop().unwrap();

        let msgs = [
            Msg::new(4, 0).ticket(t3),
            Msg::new(5, 0).unordered(),
            Msg::new(3, 10).ticket(t2),
            Msg::new(1, 20).ticket(t0),
        ];
        let handles = dispatch(&pl, msgs).await;
        assert_eq!(results(handles).await, vec![Ok(40), Ok(50), Ok(30), Ok(10)]);
        assert_eq!(sink.entered(), vec![5, 1, 3, 4]);
    }

    /// STATEMENT: The factory creates services with independent arrival sequences.
    /// HOW: A slow request in one service does not block a request in another.
    #[ntex::test]
    async fn factory() {
        let sink = Sink::default();
        let s = sink.clone();
        let factory = OrderedFactory::new(
            fn_factory(async |(): &()| Ok::<_, Infallible>(fn_service(first))),
            fn_factory(async move |(): &()| Ok::<_, Infallible>(s.clone())),
        )
        .filter(|m: &Msg| m.ordered);
        let pl1 = factory.pipeline(()).await.unwrap();
        let pl2 = factory.clone().pipeline(()).await.unwrap();

        let h1 = dispatch(&pl1, [Msg::new(1, 30), Msg::new(2, 0)]).await;
        let h2 = dispatch(&pl2, [Msg::new(3, 0)]).await;
        assert_eq!(results(h2).await, vec![Ok(30)]);
        assert_eq!(results(h1).await, vec![Ok(10), Ok(20)]);
        assert_eq!(sink.entered(), vec![3, 1, 2]);
    }
}
