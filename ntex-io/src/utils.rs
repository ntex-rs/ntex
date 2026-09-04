use std::{cell::Cell, task::Poll, task::Waker};

use ntex_service::state::{RequestState, State};
use ntex_util::task::LocalWaker;

use crate::{Filter, Io, IoBoxed, IoCallbacks};

/// Decoded item from buffer
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Decoded<T> {
    pub item: Option<T>,
    pub remains: usize,
    pub consumed: usize,
}

pub(crate) struct Extensions(Cell<Option<Box<ExtensionsInner>>>);

#[derive(Default)]
pub(crate) struct ExtensionsInner {
    disconnect: Option<Vec<LocalWaker>>,
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

    pub(super) fn notify_disconnect(&self) {
        self.with_opt(|inner| {
            if let Some(disconnect) = inner.disconnect.take() {
                for item in disconnect {
                    item.wake();
                }
            }
        });
    }

    pub(super) fn register_disconnect(&self) -> usize {
        self.with(|inner| {
            if let Some(ref mut disconnect) = inner.disconnect {
                let token = disconnect.len();
                disconnect.push(LocalWaker::default());
                token
            } else {
                inner.disconnect = Some(vec![LocalWaker::default()]);
                0
            }
        })
    }

    pub(super) fn poll_disconnect(&self, token: usize, waker: &Waker) -> Poll<()> {
        self.with(|inner| {
            if let Some(ref mut disconnect) = inner.disconnect {
                disconnect[token].register(waker);
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
    }

    pub(super) fn register_filter_callbacks<T: IoCallbacks + 'static>(&self, cb: T) {
        self.with(|inner| {
            inner.callbacks = Some(Box::new(cb));
        });
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
        assert_eq!(
            std::future::poll_fn(|cx| NullFilter.poll_read_ready(cx)).await,
            crate::Readiness::Terminate
        );
        assert_eq!(
            std::future::poll_fn(|cx| NullFilter.poll_write_ready(cx)).await,
            crate::Readiness::Terminate
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
