use std::{cell::Cell, task::Poll, task::Waker};

use ntex_service::{ServiceFactory, chain_factory, fn_service};
use ntex_util::{future::Ready, task::LocalWaker};

use crate::{Filter, Io, IoBoxed, IoCallbacks};

/// Decoded item from buffer
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Decoded<T> {
    pub item: Option<T>,
    pub remains: usize,
    pub consumed: usize,
}

/// Service that converts any `Io<F>` stream to `IoBoxed` stream
pub fn seal<F, S, C>(
    srv: S,
) -> impl ServiceFactory<
    Io<F>,
    C,
    Response = S::Response,
    Error = S::Error,
    InitError = S::InitError,
>
where
    F: Filter,
    S: ServiceFactory<IoBoxed, C>,
    C: Clone,
{
    chain_factory(fn_service(|io: Io<F>| Ready::Ok(io.boxed())))
        .map_init_err(|()| unreachable!())
        .and_then(srv)
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

#[cfg(test)]
mod tests {
    use ntex_bytes::{BytePageSize, Bytes};
    use ntex_codec::BytesCodec;
    use ntex_service::cfg::SharedCfg;

    use super::*;
    use crate::{buf::Stack, filter::NullFilter, testing::IoTest};

    #[ntex::test]
    async fn test_utils() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("REQ");

        let svc = seal(fn_service(|io: IoBoxed| async move {
            let t = io.recv(&BytesCodec).await.unwrap().unwrap();
            assert_eq!(t, b"REQ".as_ref());
            io.send(Bytes::from_static(b"RES"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, ()>(())
        }))
        .pipeline(())
        .await
        .unwrap();
        let _ = svc.call(Io::new(server, SharedCfg::default())).await;

        let buf = client.read().await.unwrap();
        assert_eq!(buf, b"RES".as_ref());
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
