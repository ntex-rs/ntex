//! Framed dispatcher service and related utilities
use std::collections::VecDeque;
use std::mem;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_service::{IntoService, Service};
use actix_utils::task::LocalWaker;
use actix_utils::{mpsc, oneshot};
use futures::future::ready;
use futures::{FutureExt, Sink as FutureSink, Stream};
use log::debug;

use crate::cell::Cell;
use crate::error::ServiceError;
use crate::item::Item;
use crate::sink::Sink;

type Request<S, U> = Item<S, U>;
type Response<U> = <U as Encoder>::Item;

pub(crate) enum FramedMessage<T> {
    Message(T),
    Close,
    WaitClose(oneshot::Sender<()>),
}

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
#[pin_project::pin_project]
pub(crate) struct FramedDispatcher<St, S, T, U>
where
    St: Clone,
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    service: S,
    sink: Sink<<U as Encoder>::Item>,
    state: St,
    dispatch_state: FramedState<S, U>,
    framed: Framed<T, U>,
    rx: Option<mpsc::Receiver<FramedMessage<<U as Encoder>::Item>>>,
    inner: Cell<FramedDispatcherInner<<U as Encoder>::Item, S::Error>>,
    disconnect: Option<Rc<dyn Fn(&mut St, bool)>>,
}

impl<St, S, T, U> FramedDispatcher<St, S, T, U>
where
    St: Clone,
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    pub(crate) fn new<F: IntoService<S>>(
        framed: Framed<T, U>,
        state: St,
        service: F,
        rx: mpsc::Receiver<FramedMessage<<U as Encoder>::Item>>,
        sink: Sink<<U as Encoder>::Item>,
        disconnect: Option<Rc<dyn Fn(&mut St, bool)>>,
    ) -> Self {
        FramedDispatcher {
            framed,
            state,
            sink,
            disconnect,
            rx: Some(rx),
            service: service.into_service(),
            dispatch_state: FramedState::Processing,
            inner: Cell::new(FramedDispatcherInner {
                buf: VecDeque::new(),
                task: LocalWaker::new(),
            }),
        }
    }
}

enum FramedState<S: Service, U: Encoder + Decoder> {
    Processing,
    Error(ServiceError<S::Error, U>),
    FramedError(ServiceError<S::Error, U>),
    FlushAndStop(Vec<oneshot::Sender<()>>),
    Stopping,
}

impl<S: Service, U: Encoder + Decoder> FramedState<S, U> {
    fn stop(&mut self, tx: Option<oneshot::Sender<()>>) {
        match self {
            FramedState::FlushAndStop(ref mut vec) => {
                if let Some(tx) = tx {
                    vec.push(tx)
                }
            }
            FramedState::Processing => {
                *self = FramedState::FlushAndStop(if let Some(tx) = tx {
                    vec![tx]
                } else {
                    Vec::new()
                })
            }
            FramedState::Error(_) | FramedState::FramedError(_) | FramedState::Stopping => {
                if let Some(tx) = tx {
                    let _ = tx.send(());
                }
            }
        }
    }
}

struct FramedDispatcherInner<I, E> {
    buf: VecDeque<Result<I, E>>,
    task: LocalWaker,
}

impl<St, S, T, U> FramedDispatcher<St, S, T, U>
where
    St: Clone,
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    pub(crate) fn poll(
        &mut self,
        cx: &mut Context,
    ) -> Poll<Result<(), ServiceError<S::Error, U>>> {
        let this = self;
        unsafe { this.inner.get_ref().task.register(cx.waker()) };

        poll(
            cx,
            &mut this.service,
            &mut this.state,
            &mut this.sink,
            &mut this.framed,
            &mut this.dispatch_state,
            &mut this.rx,
            &mut this.inner,
            &mut this.disconnect,
        )
    }
}

fn poll<St, S, T, U>(
    cx: &mut Context,
    srv: &mut S,
    state: &mut St,
    sink: &mut Sink<<U as Encoder>::Item>,
    framed: &mut Framed<T, U>,
    dispatch_state: &mut FramedState<S, U>,
    rx: &mut Option<mpsc::Receiver<FramedMessage<<U as Encoder>::Item>>>,
    inner: &mut Cell<FramedDispatcherInner<<U as Encoder>::Item, S::Error>>,
    disconnect: &mut Option<Rc<dyn Fn(&mut St, bool)>>,
) -> Poll<Result<(), ServiceError<S::Error, U>>>
where
    St: Clone,
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    match mem::replace(dispatch_state, FramedState::Processing) {
        FramedState::Processing => {
            if poll_read(cx, srv, state, sink, framed, dispatch_state, inner)
                || poll_write(cx, framed, dispatch_state, rx, inner)
            {
                poll(
                    cx,
                    srv,
                    state,
                    sink,
                    framed,
                    dispatch_state,
                    rx,
                    inner,
                    disconnect,
                )
            } else {
                Poll::Pending
            }
        }
        FramedState::Error(err) => {
            if framed.is_write_buf_empty()
                || (poll_write(cx, framed, dispatch_state, rx, inner)
                    || framed.is_write_buf_empty())
            {
                if let Some(ref disconnect) = disconnect {
                    (&*disconnect)(&mut *state, true);
                }
                Poll::Ready(Err(err))
            } else {
                *dispatch_state = FramedState::Error(err);
                Poll::Pending
            }
        }
        FramedState::FlushAndStop(mut vec) => {
            if !framed.is_write_buf_empty() {
                match Pin::new(framed).poll_flush(cx) {
                    Poll::Ready(Err(err)) => {
                        debug!("Error sending data: {:?}", err);
                    }
                    Poll::Pending => {
                        *dispatch_state = FramedState::FlushAndStop(vec);
                        return Poll::Pending;
                    }
                    Poll::Ready(_) => (),
                }
            };
            for tx in vec.drain(..) {
                let _ = tx.send(());
            }
            if let Some(ref disconnect) = disconnect {
                (&*disconnect)(&mut *state, false);
            }
            Poll::Ready(Ok(()))
        }
        FramedState::FramedError(err) => {
            if let Some(ref disconnect) = disconnect {
                (&*disconnect)(&mut *state, true);
            }
            Poll::Ready(Err(err))
        }
        FramedState::Stopping => {
            if let Some(ref disconnect) = disconnect {
                (&*disconnect)(&mut *state, false);
            }
            Poll::Ready(Ok(()))
        }
    }
}

fn poll_read<St, S, T, U>(
    cx: &mut Context,
    srv: &mut S,
    state: &mut St,
    sink: &mut Sink<<U as Encoder>::Item>,
    framed: &mut Framed<T, U>,
    dispatch_state: &mut FramedState<S, U>,
    inner: &mut Cell<FramedDispatcherInner<<U as Encoder>::Item, S::Error>>,
) -> bool
where
    St: Clone,
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    loop {
        match srv.poll_ready(cx) {
            Poll::Ready(Ok(_)) => {
                let item = match framed.next_item(cx) {
                    Poll::Ready(Some(Ok(el))) => el,
                    Poll::Ready(Some(Err(err))) => {
                        *dispatch_state = FramedState::FramedError(ServiceError::Decoder(err));
                        return true;
                    }
                    Poll::Pending => return false,
                    Poll::Ready(None) => {
                        log::trace!("Client disconnected");
                        *dispatch_state = FramedState::Stopping;
                        return true;
                    }
                };

                let mut cell = inner.clone();
                actix_rt::spawn(srv.call(Item::new(state.clone(), sink.clone(), item)).then(
                    move |item| {
                        let item = match item {
                            Ok(Some(item)) => Ok(item),
                            Ok(None) => return ready(()),
                            Err(err) => Err(err),
                        };
                        unsafe {
                            let inner = cell.get_mut();
                            inner.buf.push_back(item);
                            inner.task.wake();
                        }
                        ready(())
                    },
                ));
            }
            Poll::Pending => return false,
            Poll::Ready(Err(err)) => {
                *dispatch_state = FramedState::Error(ServiceError::Service(err));
                return true;
            }
        }
    }
}

/// write to framed object
fn poll_write<St, S, T, U>(
    cx: &mut Context,
    framed: &mut Framed<T, U>,
    dispatch_state: &mut FramedState<S, U>,
    rx: &mut Option<mpsc::Receiver<FramedMessage<<U as Encoder>::Item>>>,
    inner: &mut Cell<FramedDispatcherInner<<U as Encoder>::Item, S::Error>>,
) -> bool
where
    S: Service<Request = Request<St, U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    let inner = unsafe { inner.get_mut() };
    let mut rx_done = rx.is_none();
    let mut buf_empty = inner.buf.is_empty();
    loop {
        while !framed.is_write_buf_full() {
            if !buf_empty {
                match inner.buf.pop_front().unwrap() {
                    Ok(msg) => {
                        if let Err(err) = framed.write(msg) {
                            *dispatch_state =
                                FramedState::FramedError(ServiceError::Encoder(err));
                            return true;
                        }
                        buf_empty = inner.buf.is_empty();
                    }
                    Err(err) => {
                        *dispatch_state = FramedState::Error(ServiceError::Service(err));
                        return true;
                    }
                }
            }

            if !rx_done && rx.is_some() {
                match Pin::new(rx.as_mut().unwrap()).poll_next(cx) {
                    Poll::Ready(Some(FramedMessage::Message(msg))) => {
                        if let Err(err) = framed.write(msg) {
                            *dispatch_state =
                                FramedState::FramedError(ServiceError::Encoder(err));
                            return true;
                        }
                    }
                    Poll::Ready(Some(FramedMessage::Close)) => {
                        dispatch_state.stop(None);
                        return true;
                    }
                    Poll::Ready(Some(FramedMessage::WaitClose(tx))) => {
                        dispatch_state.stop(Some(tx));
                        return true;
                    }
                    Poll::Ready(None) => {
                        rx_done = true;
                        let _ = rx.take();
                    }
                    Poll::Pending => rx_done = true,
                }
            }

            if rx_done && buf_empty {
                break;
            }
        }

        if !framed.is_write_buf_empty() {
            match framed.flush(cx) {
                Poll::Pending => break,
                Poll::Ready(Err(err)) => {
                    debug!("Error sending data: {:?}", err);
                    *dispatch_state = FramedState::FramedError(ServiceError::Encoder(err));
                    return true;
                }
                Poll::Ready(_) => (),
            }
        } else {
            break;
        }
    }

    false
}
