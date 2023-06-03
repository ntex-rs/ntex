use std::{future::Future, marker, pin::Pin, rc::Rc, task::Context, task::Poll};

use crate::Service;

pub struct Ctx<S: ?Sized> {
    _t: marker::PhantomData<Rc<S>>,
}

impl<S: ?Sized> Ctx<S> {
    pub(crate) fn call<'a, T, R>(&self, svc: &'a T, req: R) -> T::Future<'a>
    where
        T: Service<R> + ?Sized,
        R: 'a,
    {
        svc.call(
            req,
            Ctx {
                _t: marker::PhantomData,
            },
        )
    }

    pub fn call_when_ready<'a, T, R>(&self, svc: &'a T, req: R) -> ServiceCall<'a, T, R>
    where
        T: Service<R> + ?Sized,
        R: 'a,
    {
        ServiceCall {
            svc,
            state: ServiceCallState::Ready { req: Some(req) },
        }
    }
}

pin_project_lite::pin_project! {
    pub struct ServiceCall<'a, T, Req>
    where
        T: Service<Req>,
        T: 'a,
        T: ?Sized,
        Req: 'a,
    {
        svc: &'a T,
        #[pin]
        state: ServiceCallState<'a, T, Req>,
    }
}

pin_project_lite::pin_project! {
    #[project = ServiceCallStateProject]
    enum ServiceCallState<'a, T, Req>
    where
        T: Service<Req>,
        T: 'a,
        T: ?Sized,
        Req: 'a,
    {
        Ready { req: Option<Req> },
        Call { #[pin] fut: T::Future<'a> },
        Empty,
    }
}

impl<'a, T, Req> Future for ServiceCall<'a, T, Req>
where
    T: Service<Req> + ?Sized,
{
    type Output = Result<T::Response, T::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            ServiceCallStateProject::Ready { req } => match this.svc.poll_ready(cx)? {
                Poll::Ready(()) => {
                    let fut = this.svc.call(
                        req.take().unwrap(),
                        Ctx {
                            _t: marker::PhantomData,
                        },
                    );
                    this.state.set(ServiceCallState::Call { fut });
                    self.poll(cx)
                }
                Poll::Pending => Poll::Pending,
            },
            ServiceCallStateProject::Call { fut } => fut.poll(cx).map(|r| {
                this.state.set(ServiceCallState::Empty);
                r
            }),
            ServiceCallStateProject::Empty => {
                panic!("future must not be polled after it returned `Poll::Ready`")
            }
        }
    }
}
