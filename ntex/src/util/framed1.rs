//! Framed dispatcher service and related utilities
#![allow(type_alias_bounds)]
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, mem};
use std::time::Duration;

use futures::{Future, FutureExt, Stream};
use log::debug;

use crate::channel::mpsc;
use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use crate::service::{IntoService, Service};

type Request<U> = <U as Decoder>::Item;
type Response<U> = <U as Encoder>::Item;

/// Framed transport errors
pub enum DispatcherError<E, U: Encoder + Decoder> {
    /// Inner service error
    Service(E),
    /// Encoder parse error
    Encoder(<U as Encoder>::Error),
    /// Decoder parse error
    Decoder(<U as Decoder>::Error),
}

impl<E, U: Encoder + Decoder> From<E> for DispatcherError<E, U> {
    fn from(err: E) -> Self {
        DispatcherError::Service(err)
    }
}

impl<E, U: Encoder + Decoder> fmt::Debug for DispatcherError<E, U>
where
    E: fmt::Debug,
    <U as Encoder>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DispatcherError::Service(ref e) => {
                write!(fmt, "DispatcherError::Service({:?})", e)
            }
            DispatcherError::Encoder(ref e) => {
                write!(fmt, "DispatcherError::Encoder({:?})", e)
            }
            DispatcherError::Decoder(ref e) => {
                write!(fmt, "DispatcherError::Decoder({:?})", e)
            }
        }
    }
}

impl<E, U: Encoder + Decoder> fmt::Display for DispatcherError<E, U>
where
    E: fmt::Display,
    <U as Encoder>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DispatcherError::Service(ref e) => write!(fmt, "{}", e),
            DispatcherError::Encoder(ref e) => write!(fmt, "{:?}", e),
            DispatcherError::Decoder(ref e) => write!(fmt, "{:?}", e),
        }
    }
}

/// FramedTransport - is a future that reads frames from Framed object
/// and pass then to the service.
#[pin_project::pin_project]
pub struct Dispatcher<S, T, U, In>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
<U as Encoder>::Error: std::fmt::Debug,
    In: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    inner: InnerDispatcher<S, T, U, In>,
}

struct InnerDispatcher<S, T, U, In>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    In: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    service: S,
    state: State<S, U>,
    sink: Option<In>,
    framed: Framed<T, U>,
    rx: mpsc::Receiver<Result<<U as Encoder>::Item, S::Error>>,
    disconnect_timeout: Duration,
}

enum State<S: Service, U: Encoder + Decoder> {
    Processing,
    Error(DispatcherError<S::Error, U>),
    FramedError(DispatcherError<S::Error, U>),
    FlushAndStop,
    Stopping,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum PollResult {
    Continue,
    Pending,
}

impl<S: Service, U: Encoder + Decoder> State<S, U> {
    fn take_error(&mut self) -> DispatcherError<S::Error, U> {
        match mem::replace(self, State::Processing) {
            State::Error(err) => err,
            _ => panic!(),
        }
    }

    fn take_framed_error(&mut self) -> DispatcherError<S::Error, U> {
        match mem::replace(self, State::Processing) {
            State::FramedError(err) => err,
            _ => panic!(),
        }
    }
}

impl<S, T, U> Dispatcher<S, T, U, mpsc::Receiver<<U as Encoder>::Item>>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
{
    /// Construct new `Dispatcher` instance
    pub fn new<F: IntoService<S>>(framed: Framed<T, U>, service: F) -> Self {
        Dispatcher {
            inner: InnerDispatcher {
                framed,
                sink: None,
                rx: mpsc::channel().1,
                service: service.into_service(),
                state: State::Processing,
                disconnect_timeout: Duration::from_millis(1000),
            }
        }
    }
}

impl<S, T, U, In> Dispatcher<S, T, U, In>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    In: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    /// Construct new `Dispatcher` instance with outgoing messages stream.
    pub fn with<F: IntoService<S>>(
        framed: Framed<T, U>,
        sink: In,
        service: F,
    ) -> Self {
        Dispatcher {
            inner: InnerDispatcher {
                framed,
                sink: Some(sink),
                rx: mpsc::channel().1,
                service: service.into_service(),
                state: State::Processing,
                disconnect_timeout: Duration::from_millis(1000),
            }
        }
    }

    /// Set connection disconnect timeout in milliseconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 1 seconds.
    pub fn disconnect_timeout(mut self, val: u64) -> Self {
        self.inner.disconnect_timeout = Duration::from_millis(val);
        self
    }
}

impl<S, T, U, In> Future for Dispatcher<S, T, U, In>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    In: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    type Output = Result<(), DispatcherError<S::Error, U>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

impl<S, T, U, In> InnerDispatcher<S, T, U, In>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    In: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    fn poll_read(&mut self, cx: &mut Context<'_>) -> PollResult {
        loop {
            match self.service.poll_ready(cx) {
                Poll::Ready(Ok(_)) => {
                    let item = match self.framed.next_item(cx) {
                        Poll::Ready(Some(Ok(el))) => el,
                        Poll::Ready(Some(Err(err))) => {
                            self.state =
                                State::FramedError(DispatcherError::Decoder(err));
                            return PollResult::Continue;
                        }
                        Poll::Pending => return PollResult::Pending,
                        Poll::Ready(None) => {
                            self.state = State::Stopping;
                            return PollResult::Continue;
                        }
                    };

                    let tx = self.rx.sender();
                    crate::rt::spawn(self.service.call(item).map(move |item| {
                        let item = match item {
                            Ok(Some(item)) => Ok(item),
                            Err(e) => Err(e),
                            _ => return,
                        };
                        let _ = tx.send(item);
                    }));
                }
                Poll::Pending => return PollResult::Pending,
                Poll::Ready(Err(err)) => {
                    self.state = State::Error(DispatcherError::Service(err));
                    return PollResult::Continue;
                }
            }
        }
    }

    /// write to framed object
    fn poll_write(&mut self, cx: &mut Context<'_>) -> PollResult {
        loop {
            while !self.framed.is_write_buf_full() {
                match Pin::new(&mut self.rx).poll_next(cx) {
                    Poll::Ready(Some(Ok(msg))) => {
                        if let Err(err) = self.framed.write(msg) {
                            self.state =
                                State::FramedError(DispatcherError::Encoder(err));
                            return PollResult::Continue;
                        }
                    }
                    Poll::Ready(Some(Err(err))) => {
                        self.state = State::Error(DispatcherError::Service(err));
                        return PollResult::Continue;
                    }
                    Poll::Ready(None) | Poll::Pending => {},
                }

                if self.sink.is_some() {
                    match Pin::new(self.sink.as_mut().unwrap()).poll_next(cx) {
                        Poll::Ready(Some(msg)) => {
                            if let Err(err) = self.framed.write(msg) {
                                self.state = State::Shutdown(
                                    DispatcherError::Encoder(err),
                                );
                                return PollResult::Continue;
                            }
                            continue;
                        }
                        Poll::Ready(None) => {
                            let _ = self.sink.take();
                            self.state = State::FlushAndStop;
                            return PollResult::Continue;
                        }
                        Poll::Pending => (),
                    }
                }
                break;
            }

            if !self.framed.is_write_buf_empty() {
                match self.framed.flush(cx) {
                    Poll::Pending => break,
                    Poll::Ready(Ok(_)) => (),
                    Poll::Ready(Err(err)) => {
                        debug!("Error sending data: {:?}", err);
                        self.state = State::FramedError(DispatcherError::Encoder(err));
                        return PollResult::Continue;
                    }
                }
            } else {
                break;
            }
        }

        PollResult::Pending
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), DispatcherError<S::Error, U>>> {
        loop {
            return match self.state {
                State::Processing => {
                    if self.poll_read(cx) || self.poll_write(cx) {
                        continue;
                    } else {
                        Poll::Pending
                    }
                }
                State::Error(_) => {
                    // flush write buffer
                    if !self.framed.is_write_buf_empty() {
                        if let Poll::Pending = self.framed.flush(cx) {
                            return Poll::Pending;
                        }
                    }
                    Poll::Ready(Err(self.state.take_error()))
                }
                State::FlushAndStop => {
                    if !self.framed.is_write_buf_empty() {
                        match self.framed.flush(cx) {
                            Poll::Ready(Err(err)) => {
                                debug!("Error sending data: {:?}", err);
                                Poll::Ready(Ok(()))
                            }
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                        }
                    } else {
                        Poll::Ready(Ok(()))
                    }
                }
                State::FramedError(_) => {
                    Poll::Ready(Err(self.state.take_framed_error()))
                }
                State::Stopping => Poll::Ready(Ok(())),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};
    use futures::future::ok;

    use super::*;
    use crate::codec::{BytesCodec, Framed};
    use crate::testing::Io;

    #[ntex_rt::test]
    async fn test_basic() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let framed = Framed::new(server, BytesCodec);
        let disp = Dispatcher::new(
            framed,
            crate::fn_service(|msg: BytesMut| ok::<_, ()>(msg.freeze())),
        );
        crate::rt::spawn(disp.map(|_| ()));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        client.close().await;
        assert!(client.is_server_dropped());
    }
}
