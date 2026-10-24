use std::cell::Cell;
use std::io;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use ntex_rt::LocalWaker;
use ntex_service::state::{RequestState, State};
use ntex_util::time::{Seconds, Sleep};

use crate::waiters::{WaiterEntry, Waiters};
use crate::{Filter, Io, IoBoxed, IoCallbacks};

/// Result of a single decode attempt.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Decoded<T> {
    /// The decoded item, or `None` when the codec needs more input.
    pub item: Option<T>,
    /// Bytes left in the application-facing read buffer after the attempt.
    pub remains: usize,
    /// Bytes consumed from the read buffer by the attempt.
    pub consumed: usize,
}

// capacity of the wake buffer kept for reuse
const MAX_SPARE_WAKERS: usize = 1024;

#[derive(Default)]
pub(crate) struct Extensions {
    // tasks waiting for io events, by tag, allocated on first registration
    waiters: Cell<Option<Box<Waiters>>>,
    // filter callbacks registered for io events
    callbacks: Cell<Option<Rc<dyn IoCallbacks>>>,
}

impl Extensions {
    /// Runs `f` with the waiters, allocating them if needed.
    ///
    /// The waiters are taken out while `f` runs, so `f` must not run user
    /// code, wakers are woken and dropped by the caller after `f`.
    fn with_waiters<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Waiters) -> R,
    {
        let mut waiters = self.waiters.take().unwrap_or_default();
        let result = f(&mut waiters);
        self.waiters.set(Some(waiters));
        result
    }

    /// Runs `f` with the waiters if they are allocated, see [`Self::with_waiters`].
    fn with_waiters_opt<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut Waiters) -> R,
    {
        let mut waiters = self.waiters.take()?;
        let result = f(&mut waiters);
        self.waiters.set(Some(waiters));
        Some(result)
    }

    /// Registers the waker of the waiter, a woken waiter gets a new entry.
    pub(super) fn register_waker(&self, waiter: &WaiterEntry, waker: &Waker) {
        let replaced = self.with_waiters(|waiters| {
            if let Some(id) = waiter.id.get()
                && let Some(replaced) = waiters.update(id, waker)
            {
                replaced
            } else {
                waiter.id.set(Some(waiters.register(waiter.tag, waker)));
                None
            }
        });
        drop(replaced);
    }

    /// Registers the waiter, or reports that its registration was woken.
    ///
    /// A reported wake clears the registration, the next poll registers again.
    pub(super) fn poll_waker(&self, waiter: &WaiterEntry, waker: &Waker) -> Poll<()> {
        let (result, replaced) = self.with_waiters(|waiters| match waiter.id.get() {
            None => {
                waiter.id.set(Some(waiters.register(waiter.tag, waker)));
                (Poll::Pending, None)
            }
            Some(id) => {
                if let Some(replaced) = waiters.update(id, waker) {
                    (Poll::Pending, replaced)
                } else {
                    waiter.id.set(None);
                    (Poll::Ready(()), None)
                }
            }
        });
        drop(replaced);
        result
    }

    /// Removes the waiter entry unless it is woken.
    pub(super) fn remove_waker(&self, waiter: &WaiterEntry) {
        if let Some(id) = waiter.id.take() {
            let waker = self.with_waiters_opt(|waiters| waiters.remove(id, waiter.tag));
            drop(waker);
        }
    }

    /// Wakes and removes all wakers of the tag.
    pub(super) fn wake(&self, tag: usize) {
        self.wake_with(|waiters, wakers| waiters.take(tag, wakers));
    }

    /// Wakes and removes all wakers.
    pub(super) fn wake_all(&self) {
        self.wake_with(Waiters::take_all);
    }

    /// Collects wakers with `f` and wakes them once the waiters are put back.
    ///
    /// The wakers are collected into a per-thread buffer that is reused
    /// across wakes, a nested wake allocates its own.
    fn wake_with<F>(&self, f: F)
    where
        F: FnOnce(&mut Waiters, &mut Vec<LocalWaker>),
    {
        thread_local! {
            // boxed, so taking and returning the buffer moves a single pointer
            #[allow(clippy::box_collection)]
            static WAKERS: Cell<Option<Box<Vec<LocalWaker>>>> = const { Cell::new(None) };
        }

        let Some(mut waiters) = self.waiters.take() else {
            return;
        };
        let mut wakers = WAKERS
            .try_with(Cell::take)
            .ok()
            .flatten()
            .unwrap_or_default();
        f(&mut waiters, &mut wakers);
        self.waiters.set(Some(waiters));

        wakers.drain(..).for_each(LocalWaker::wake);
        if wakers.capacity() <= MAX_SPARE_WAKERS {
            let _ = WAKERS.try_with(|spare| spare.set(Some(wakers)));
        }
    }

    #[cfg(test)]
    pub(super) fn wakers_len(&self) -> usize {
        self.with_waiters_opt(|waiters| waiters.len()).unwrap_or(0)
    }

    pub(super) fn register_filter_callbacks<T: IoCallbacks + 'static>(&self, cb: T) {
        drop(self.callbacks.replace(Some(Rc::new(cb))));
    }

    pub(super) fn take_callbacks(&self) -> Option<Rc<dyn IoCallbacks>> {
        self.callbacks.take()
    }

    /// Calls `f` with the registered callbacks.
    ///
    /// The callbacks stay registered while `f` runs, they may be replaced
    /// or taken by `f` and are dropped once `f` returns.
    pub(crate) fn with_callbacks<F>(&self, f: F)
    where
        F: FnOnce(&dyn IoCallbacks),
    {
        let callbacks = self.callbacks.take();
        let cb = callbacks.clone();
        self.callbacks.set(callbacks);
        if let Some(cb) = cb {
            f(cb.as_ref());
        }
    }
}

impl<F> RequestState<Io<F>> for Io<F> {
    type State = ();

    #[inline]
    fn unpack(self) -> ((), Io<F>) {
        ((), self)
    }
}

impl<F: Filter> RequestState<IoBoxed> for Io<F> {
    type State = ();

    #[inline]
    fn unpack(self) -> ((), IoBoxed) {
        ((), self.boxed())
    }
}

impl RequestState<IoBoxed> for IoBoxed {
    type State = ();

    #[inline]
    fn unpack(self) -> ((), IoBoxed) {
        ((), self)
    }
}

impl<F: Filter, St: 'static> RequestState<IoBoxed> for State<St, Io<F>> {
    type State = St;

    #[inline]
    fn unpack(self) -> (St, IoBoxed) {
        let State { req, state } = self;
        (state, req.boxed())
    }
}

/// Deadline for a wait on output, started lazily on the first wait.
pub(crate) struct WriteDeadline {
    timeout: Seconds,
    sleep: Option<Sleep>,
}

impl WriteDeadline {
    /// Creates a deadline, a zero timeout never expires.
    pub(crate) fn new(timeout: Seconds) -> Self {
        Self {
            timeout,
            sleep: None,
        }
    }

    pub(crate) fn poll_expired(&mut self, cx: &mut Context<'_>) -> bool {
        if self.timeout.is_zero() {
            false
        } else {
            let timeout = self.timeout;
            self.sleep
                .get_or_insert_with(|| Sleep::new(timeout.into()))
                .poll_elapsed(cx)
                .is_ready()
        }
    }
}

pub(crate) fn write_timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Write timeout")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
    use std::task::Wake;

    use ntex_bytes::BytePageSize;
    use ntex_service::cfg::SharedCfg;

    use super::*;
    use crate::{Sealed, buf::Stack, filter::NullFilter, testing::IoTest};

    #[derive(Debug)]
    struct NoopNotify;

    impl ntex_rt::Notify for NoopNotify {
        fn notify(&self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Wrapped {
        inner: Waker,
        notifications: Arc<AtomicUsize>,
    }

    impl Wake for Wrapped {
        fn wake(self: Arc<Self>) {
            self.notifications.fetch_add(1, Ordering::Relaxed);
            self.inner.wake_by_ref();
        }
    }

    #[test]
    fn local_waiters_deliver_and_release_native_handles() {
        let rt = ntex_rt::Runtime::new(Box::new(NoopNotify));
        let ext = Rc::new(Extensions::default());
        let slots = [7, 7, crate::waiters::TAG_WRITE].map(|tag| Rc::new(WaiterEntry::new(tag)));
        let sources = slots.each_ref().map(|_| Rc::new(Cell::new(None)));
        let tasks: Vec<_> = slots
            .iter()
            .zip(&sources)
            .map(|(slot, source)| {
                let (ext, slot, source) = (ext.clone(), slot.clone(), source.clone());
                rt.spawn(std::future::poll_fn(move |cx| {
                    source.set(Some(cx.waker().clone()));
                    ext.poll_waker(&slot, cx.waker())
                }))
            })
            .collect();
        rt.poll();
        assert_eq!(ext.wakers_len(), 3);

        let notifications = Arc::new(AtomicUsize::new(0));
        let wrapped = Waker::from(Arc::new(Wrapped {
            inner: sources[0].take().unwrap(),
            notifications: notifications.clone(),
        }));
        ext.register_waker(&slots[0], &wrapped);
        drop(wrapped);
        ext.wake(7);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert_eq!(ext.wakers_len(), 1);
        rt.poll();
        assert!(tasks[0].is_finished());
        assert!(tasks[1].is_finished());
        assert!(!tasks[2].is_finished());
        assert!(slots[0].id.get().is_none());
        assert!(slots[1].id.get().is_none());

        ext.remove_waker(&slots[2]);
        ext.wake_all();
        rt.poll();
        assert!(!tasks[2].is_finished());
        assert_eq!(ext.wakers_len(), 0);
        sources[2].take().unwrap().wake();
        rt.poll();
        assert_eq!(ext.wakers_len(), 1);

        for source in sources {
            drop(source.take());
        }
        drop(rt);
        for task in &tasks {
            assert!(task.is_finished());
        }
        drop(tasks);
        assert_eq!(Rc::strong_count(&ext), 1);
        ext.wake_all();
        assert_eq!(ext.wakers_len(), 0);
    }

    #[test]
    fn local_waiter_wake_unwind_releases_native_handles() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        thread_local! {
            static ON_WAKE: Cell<Option<Box<dyn FnOnce()>>> = Cell::new(None);
        }
        struct Reenter;

        impl Wake for Reenter {
            fn wake(self: Arc<Self>) {
                ON_WAKE.with(Cell::take).unwrap()();
            }
        }

        let rt = ntex_rt::Runtime::new(Box::new(NoopNotify));
        let ext = Rc::new(Extensions::default());
        let slot = Rc::new(WaiterEntry::new(7));
        let nested = Rc::new(WaiterEntry::new(8));
        let (task_ext, task_slot) = (ext.clone(), slot.clone());
        rt.spawn(std::future::poll_fn(move |cx| {
            task_ext.register_waker(&task_slot, cx.waker());
            Poll::<()>::Pending
        }))
        .detach();
        rt.poll();

        let notifications = Arc::new(AtomicUsize::new(0));
        let wrapped = Waker::from(Arc::new(Wrapped {
            inner: Waker::noop().clone(),
            notifications: notifications.clone(),
        }));
        let (callback_ext, callback_nested) = (ext.clone(), nested.clone());
        ON_WAKE.set(Some(Box::new(move || {
            assert_eq!(callback_ext.wakers_len(), 0);
            callback_ext.register_waker(&callback_nested, &wrapped);
            panic!("wake");
        })));
        // The newest waiter wakes first; unwinding drops the extracted native handle.
        ext.register_waker(&WaiterEntry::new(7), &Waker::from(Arc::new(Reenter)));
        assert!(catch_unwind(AssertUnwindSafe(|| ext.wake(7))).is_err());
        assert_eq!(ext.wakers_len(), 1);
        rt.poll();
        assert_eq!(Rc::strong_count(&slot), 1, "orphaned task was not released");
        ext.remove_waker(&slot);
        assert_eq!(ext.wakers_len(), 1);
        ext.wake(8);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert_eq!(ext.wakers_len(), 0);
        assert!(ext.poll_waker(&nested, Waker::noop()).is_ready());
        assert!(ext.poll_waker(&nested, Waker::noop()).is_pending());
        ext.wake_all();
        assert_eq!(ext.wakers_len(), 0);
    }

    #[test]
    fn local_registrations_across_runtime_shutdown() {
        use std::future::poll_fn;

        for wrapped in [false, true] {
            let notifications = Arc::new(AtomicUsize::new(0));
            let count = notifications.clone();
            let registrations = ntex::rt::System::build()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let registrations = Rc::new((
                        ntex_util::task::LocalWaker::new(),
                        Sleep::new(Seconds(60).into()),
                        Extensions::default(),
                        WaiterEntry::new(7),
                    ));
                    let polls = Rc::new(Cell::new(0));
                    let (saved, task_polls) = (registrations.clone(), polls.clone());
                    let _task = ntex_rt::spawn(poll_fn(move |cx| {
                        let waker = if wrapped {
                            Waker::from(Arc::new(Wrapped {
                                inner: cx.waker().clone(),
                                notifications: count.clone(),
                            }))
                        } else {
                            cx.waker().clone()
                        };
                        saved.0.register(&waker);
                        if saved.1.is_elapsed() {
                            saved.1.reset(Seconds(60));
                        }
                        assert!(
                            saved
                                .1
                                .poll_elapsed(&mut Context::from_waker(&waker))
                                .is_pending()
                        );
                        saved.2.register_waker(&saved.3, &waker);
                        task_polls.set(task_polls.get() + 1);
                        Poll::<()>::Pending
                    }));
                    for expected in 1..=4 {
                        while polls.get() < expected {
                            ntex_util::task::yield_to().await;
                        }
                        assert_eq!(polls.get(), expected);
                        match expected {
                            1 => {
                                let standard = registrations.0.take().unwrap();
                                std::thread::spawn(move || standard.wake()).join().unwrap();
                            }
                            2 => registrations.1.elapse(),
                            3 => registrations.2.wake(7),
                            _ => {}
                        }
                    }
                    registrations
                });
            assert_eq!(
                Rc::strong_count(&registrations),
                1,
                "future survived shutdown"
            );
            assert!(registrations.1.is_elapsed());
            assert!(registrations.0.wake_checked());
            registrations.2.wake_all();
            assert_eq!(registrations.2.wakers_len(), 0);
            assert_eq!(
                notifications.load(Ordering::Relaxed),
                if wrapped { 5 } else { 0 }
            );
        }
    }

    #[ntex::test]
    async fn test_null_filter() {
        let (_, server) = IoTest::create();
        let io = Io::new(server, SharedCfg::default());
        let ioref = io.get_ref();
        let stack = Stack::new(BytePageSize::Size16);
        assert!(NullFilter.query(std::any::TypeId::of::<()>()).is_none());
        assert!(
            stack
                .with_filter(&ioref, |ctx| NullFilter.shutdown(ctx))
                .unwrap()
                .is_ready()
        );
        // `IoContext` closes the connection once the chain is gone, the
        // placeholder never pauses it.
        assert!(matches!(
            std::future::poll_fn(|cx| NullFilter.poll_ready(cx)).await,
            Ok(())
        ));
        assert!(
            stack
                .with_filter(&ioref, |ctx| NullFilter.process_write_buf(ctx))
                .is_ok()
        );
        assert_eq!(
            stack.with_filter(&ioref, |ctx| NullFilter.process_read_buf(ctx).unwrap()),
            ()
        );
    }

    #[ntex::test]
    async fn request_state_unpack() {
        use ntex_service::state::{RequestState, State};

        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let id = io.id();

        let ((), io) = <Io as RequestState<Io>>::unpack(io);
        assert_eq!(io.id(), id);
        let ((), io) = <Io as RequestState<IoBoxed>>::unpack(io);
        assert_eq!(io.id(), id);
        let ((), mut io) = <IoBoxed as RequestState<IoBoxed>>::unpack(io);
        assert_eq!(io.id(), id);

        let st = State {
            req: Io::<Sealed>::from(io.take()),
            state: 10u32,
        };
        let (state, io) = <_ as RequestState<IoBoxed>>::unpack(st);
        assert_eq!(state, 10);
        assert_eq!(io.id(), id);
    }
}
