use std::{future::Future, future::poll_fn, pin, pin::Pin, task::Poll};

use crate::{Ctx, Service};

pub(crate) type BoxFuture<'a, R> = Pin<Box<dyn Future<Output = R> + 'a>>;

pub(crate) async fn shutdown<S, St, A, B, RA, RB>(svc1: &A, svc2: &B, ctx: Ctx<'_, S, St>)
where
    A: Service<St, RA>,
    B: Service<St, RB>,
{
    let mut fut1 = pin::pin!(ctx.shutdown(svc1));
    let mut fut2 = pin::pin!(ctx.shutdown(svc2));

    let mut ready1 = false;
    let mut ready2 = false;

    poll_fn(move |cx| {
        if !ready1 && Pin::new(&mut fut1).poll(cx).is_ready() {
            ready1 = true;
        }
        if !ready2 && Pin::new(&mut fut2).poll(cx).is_ready() {
            ready2 = true;
        }
        if ready1 && ready2 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

pub(crate) async fn ready<S, St, A, B, RA, RB>(
    svc1: &A,
    svc2: &B,
    ctx: Ctx<'_, S, St>,
) -> Result<(), A::Error>
where
    A: Service<St, RA>,
    B: Service<St, RB, Error = A::Error>,
{
    let mut fut1 = pin::pin!(ctx.ready(svc1));
    let mut fut2 = pin::pin!(ctx.ready(svc2));

    let mut ready1 = false;
    let mut ready2 = false;

    poll_fn(move |cx| {
        if !ready1 && Pin::new(&mut fut1).poll(cx)?.is_ready() {
            ready1 = true;
        }
        if !ready2 && Pin::new(&mut fut2).poll(cx)?.is_ready() {
            ready2 = true;
        }
        if ready1 && ready2 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    })
    .await
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{cell::Cell, cell::RefCell, future::poll_fn, rc::Rc, task::Poll, task::Waker};

    use ntex::channel::oneshot::{self, Receiver, Sender};
    use ntex::time::{Millis, sleep};

    use crate::{Ctx, Pipeline, Service};

    pub(crate) type Req = (Receiver<()>, Receiver<()>);

    #[derive(Default)]
    pub(crate) struct State {
        pub(crate) active: Cell<usize>,
        pub(crate) max: Cell<usize>,
        pub(crate) checks: Cell<usize>,
        waker: RefCell<Option<Waker>>,
    }

    /// Accepts one call at a time
    pub(crate) struct Single(pub(crate) Rc<State>);

    impl Service<(), Receiver<()>> for Single {
        type Res = ();
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), ()> {
            self.0.checks.set(self.0.checks.get() + 1);
            poll_fn(|cx| {
                if self.0.active.get() >= 1 {
                    *self.0.waker.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            })
            .await
        }

        async fn call(&self, rx: Receiver<()>, _: Ctx<'_, Self>) -> Result<(), ()> {
            let st = &self.0;
            st.active.set(st.active.get() + 1);
            st.max.set(st.max.get().max(st.active.get()));
            let _ = rx.await;
            st.active.set(st.active.get() - 1);
            if let Some(w) = st.waker.borrow_mut().take() {
                w.wake();
            }
            Ok(())
        }
    }

    /// Starts a call; the first sender releases the first step,
    /// the second sender releases `Single`
    pub(crate) fn start(pl: &Pipeline<Req, (), ()>) -> (Sender<()>, Sender<()>) {
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        let pl = pl.bind();
        ntex::rt::spawn(async move {
            let _ = pl.call((rx1, rx2)).await;
        });
        (tx1, tx2)
    }

    pub(crate) async fn tick() {
        sleep(Millis(20)).await;
    }

    /// Readiness reported for one call must not be used after another call
    /// entered the service
    pub(crate) async fn concurrent_entry(pl: &Pipeline<Req, (), ()>, st: &State) {
        let (x1, _x2) = start(pl);
        tick().await; // X: ready, waits in the first step
        let (z1, z2) = start(pl);
        let _ = z1.send(());
        tick().await; // Z enters the service
        assert_eq!(st.active.get(), 1);
        let (y1, _y2) = start(pl);
        tick().await; // Y: readiness check waits for the service
        let _ = x1.send(());
        tick().await; // X: waits for the service readiness
        let _ = z2.send(());
        tick().await; // Z leaves: Y's check completes, then X enters
        assert_eq!(st.active.get(), 1);
        let _ = y1.send(());
        tick().await; // Y leaves the first step
        assert_eq!(st.max.get(), 1, "service capacity exceeded");
    }
}
