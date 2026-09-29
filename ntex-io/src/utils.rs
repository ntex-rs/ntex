use std::cell::Cell;
use std::io;
use std::task::{Context, Poll, Waker};

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

pub(crate) struct Extensions(Cell<Option<Box<ExtensionsInner>>>);

#[derive(Default)]
pub(crate) struct ExtensionsInner {
    // tasks waiting for io events, by tag
    waiters: Waiters,
    // filter callbacks registered for io events
    pub(crate) callbacks: Option<Box<dyn IoCallbacks>>,
}

impl Default for Extensions {
    fn default() -> Extensions {
        Extensions(Cell::new(None))
    }
}

impl Extensions {
    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut ExtensionsInner) -> R,
    {
        let mut inner = if let Some(inner) = self.0.take() {
            inner
        } else {
            Box::new(ExtensionsInner::default())
        };
        let result = f(&mut inner);
        self.0.set(Some(inner));
        result
    }

    fn with_opt<F>(&self, f: F)
    where
        F: FnOnce(&mut ExtensionsInner),
    {
        if let Some(mut inner) = self.0.take() {
            f(&mut inner);
            self.0.set(Some(inner));
        }
    }

    /// Registers the waker of the waiter, a woken waiter gets a new entry.
    pub(super) fn register_waker(&self, waiter: &WaiterEntry, waker: &Waker) {
        self.with(|inner| {
            let waiters = &mut inner.waiters;
            if !waiter.id.get().is_some_and(|id| waiters.update(id, waker)) {
                waiter.id.set(Some(waiters.register(waiter.tag, waker)));
            }
        });
    }

    /// Registers the waiter, or reports that its registration was woken.
    ///
    /// A reported wake clears the registration, the next poll registers again.
    pub(super) fn poll_waker(&self, waiter: &WaiterEntry, waker: &Waker) -> Poll<()> {
        self.with(|inner| {
            let waiters = &mut inner.waiters;
            match waiter.id.get() {
                None => {
                    waiter.id.set(Some(waiters.register(waiter.tag, waker)));
                    Poll::Pending
                }
                Some(id) if waiters.update(id, waker) => Poll::Pending,
                Some(_) => {
                    waiter.id.set(None);
                    Poll::Ready(())
                }
            }
        })
    }

    /// Removes the waiter entry unless it is woken.
    pub(super) fn remove_waker(&self, waiter: &WaiterEntry) {
        if let Some(id) = waiter.id.take() {
            self.with_opt(|inner| inner.waiters.remove(id, waiter.tag));
        }
    }

    /// Wakes and removes all wakers of the tag.
    pub(super) fn wake(&self, tag: usize) {
        self.with_opt(|inner| inner.waiters.wake(tag));
    }

    /// Wakes and removes all wakers.
    pub(super) fn wake_all(&self) {
        self.with_opt(|inner| inner.waiters.wake_all());
    }

    #[cfg(test)]
    pub(super) fn wakers_len(&self) -> usize {
        let mut len = 0;
        self.with_opt(|inner| len = inner.waiters.len());
        len
    }

    pub(super) fn register_filter_callbacks<T: IoCallbacks + 'static>(&self, cb: T) {
        self.with(|inner| {
            inner.callbacks = Some(Box::new(cb));
        });
    }

    pub(super) fn take_callbacks(&self) -> Option<Box<dyn IoCallbacks>> {
        let mut callbacks = None;
        self.with_opt(|inner| callbacks = inner.callbacks.take());
        callbacks
    }

    pub(crate) fn with_callbacks<F>(&self, f: F)
    where
        F: FnOnce(&dyn IoCallbacks),
    {
        self.with_opt(|inner| {
            if let Some(ref cb) = inner.callbacks {
                f(cb.as_ref());
            }
        });
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
    use ntex_bytes::BytePageSize;
    use ntex_service::cfg::SharedCfg;

    use super::*;
    use crate::{buf::Stack, filter::NullFilter, testing::IoTest};

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
        // The chain is gone, so the transport closes the connection
        // gracefully. `IoContext` escalates to `Terminate` when the connection
        // was force-closed; `NullFilter` itself cannot see that state.
        assert_eq!(
            std::future::poll_fn(|cx| NullFilter.poll_read_ready(cx)).await,
            crate::Readiness::Close
        );
        assert_eq!(
            std::future::poll_fn(|cx| NullFilter.poll_write_ready(cx)).await,
            crate::Readiness::Close
        );
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
}
