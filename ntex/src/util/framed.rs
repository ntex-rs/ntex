//! Framed transport dispatcher
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::{ready, FutureExt, Stream};
use log::debug;

use crate::channel::mpsc;
use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use crate::rt::time::{delay_for, Delay};
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
pub struct Dispatcher<S, T, U, Out>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    inner: InnerDispatcher<S, T, U, Out>,
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
                state: FramedState::Processing,
                disconnect_timeout: 1000,
            },
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
        sink: Option<In>,
        service: F,
    ) -> Self {
        Dispatcher {
            inner: InnerDispatcher {
                framed,
                sink,
                rx: mpsc::channel().1,
                service: service.into_service(),
                state: FramedState::Processing,
                disconnect_timeout: 1000,
            },
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
        self.inner.disconnect_timeout = val;
        self
    }

    pub(crate) fn poll_inner(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), DispatcherError<S::Error, U>>> {
        self.inner.poll(cx)
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

enum FramedState<S: Service, U: Encoder + Decoder> {
    Processing,
    FlushAndStop(Option<DispatcherError<S::Error, U>>),
    Shutdown(Option<DispatcherError<S::Error, U>>),
    ShutdownIo(Delay, Option<Result<(), DispatcherError<S::Error, U>>>),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum PollResult {
    Continue,
    Pending,
}

struct InnerDispatcher<S, T, U, Out>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    service: S,
    sink: Option<Out>,
    state: FramedState<S, U>,
    framed: Framed<T, U>,
    rx: mpsc::Receiver<Result<<U as Encoder>::Item, S::Error>>,
    disconnect_timeout: u64,
}

impl<S, T, U, Out> InnerDispatcher<S, T, U, Out>
where
    S: Service<Request = Request<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite + Unpin,
    U: Decoder + Encoder,
    <U as Encoder>::Item: 'static,
    <U as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <U as Encoder>::Item> + Unpin,
{
    fn poll_read(&mut self, cx: &mut Context<'_>) -> PollResult {
        loop {
            match self.service.poll_ready(cx) {
                Poll::Ready(Ok(_)) => {
                    let item = match self.framed.next_item(cx) {
                        Poll::Ready(Some(Ok(el))) => el,
                        Poll::Ready(Some(Err(err))) => {
                            self.state = FramedState::Shutdown(Some(
                                DispatcherError::Decoder(err),
                            ));
                            return PollResult::Continue;
                        }
                        Poll::Pending => return PollResult::Pending,
                        Poll::Ready(None) => {
                            log::trace!("Client disconnected");
                            self.state = FramedState::Shutdown(None);
                            return PollResult::Continue;
                        }
                    };

                    let tx = self.rx.sender();
                    crate::rt::spawn(self.service.call(item).map(move |item| {
                        let item = match item {
                            Ok(Some(item)) => Ok(item),
                            Err(err) => Err(err),
                            _ => return,
                        };
                        let _ = tx.send(item);
                    }));
                }
                Poll::Pending => return PollResult::Pending,
                Poll::Ready(Err(err)) => {
                    self.state =
                        FramedState::FlushAndStop(Some(DispatcherError::Service(err)));
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
                            self.state = FramedState::Shutdown(Some(
                                DispatcherError::Encoder(err),
                            ));
                            return PollResult::Continue;
                        }
                        continue;
                    }
                    Poll::Ready(Some(Err(err))) => {
                        self.state = FramedState::FlushAndStop(Some(
                            DispatcherError::Service(err),
                        ));
                        return PollResult::Continue;
                    }
                    Poll::Ready(None) | Poll::Pending => {}
                }

                if let Some(ref mut sink) = self.sink {
                    match Pin::new(sink).poll_next(cx) {
                        Poll::Ready(Some(msg)) => {
                            if let Err(err) = self.framed.write(msg) {
                                self.state = FramedState::Shutdown(Some(
                                    DispatcherError::Encoder(err),
                                ));
                                return PollResult::Continue;
                            }
                            continue;
                        }
                        Poll::Ready(None) => {
                            let _ = self.sink.take();
                            self.state = FramedState::FlushAndStop(None);
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
                        self.state =
                            FramedState::Shutdown(Some(DispatcherError::Encoder(err)));
                        return PollResult::Continue;
                    }
                }
            } else {
                break;
            }
        }
        PollResult::Pending
    }

    pub(super) fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), DispatcherError<S::Error, U>>> {
        loop {
            match self.state {
                FramedState::Processing => {
                    let read = self.poll_read(cx);
                    let write = self.poll_write(cx);
                    if read == PollResult::Continue || write == PollResult::Continue {
                        continue;
                    } else {
                        return Poll::Pending;
                    }
                }
                FramedState::FlushAndStop(ref mut err) => {
                    // drain service responses
                    match Pin::new(&mut self.rx).poll_next(cx) {
                        Poll::Ready(Some(Ok(msg))) => {
                            if let Err(err) = self.framed.write(msg) {
                                self.state = FramedState::Shutdown(Some(
                                    DispatcherError::Encoder(err),
                                ));
                                continue;
                            }
                        }
                        Poll::Ready(Some(Err(err))) => {
                            self.state = FramedState::Shutdown(Some(err.into()));
                            continue;
                        }
                        Poll::Ready(None) | Poll::Pending => (),
                    }

                    // flush io
                    if !self.framed.is_write_buf_empty() {
                        match self.framed.flush(cx) {
                            Poll::Ready(Err(err)) => {
                                debug!("Error sending data: {:?}", err);
                            }
                            Poll::Pending => {
                                return Poll::Pending;
                            }
                            Poll::Ready(_) => (),
                        }
                    };
                    self.state = FramedState::Shutdown(err.take());
                }
                FramedState::Shutdown(ref mut err) => {
                    return if self.service.poll_shutdown(cx, err.is_some()).is_ready() {
                        let result = if let Some(err) = err.take() {
                            if let DispatcherError::Service(_) = err {
                                Err(err)
                            } else {
                                // no need for io shutdown because io error occured
                                return Poll::Ready(Err(err));
                            }
                        } else {
                            Ok(())
                        };

                        // frame close, closes io WR side and waits for disconnect
                        // on read side. we need disconnect timeout, because it
                        // could hang forever.
                        let pending = self.framed.close(cx).is_pending();
                        if self.disconnect_timeout != 0 && pending {
                            self.state = FramedState::ShutdownIo(
                                delay_for(Duration::from_millis(
                                    self.disconnect_timeout,
                                )),
                                Some(result),
                            );
                            continue;
                        } else {
                            Poll::Ready(result)
                        }
                    } else {
                        Poll::Pending
                    };
                }
                FramedState::ShutdownIo(ref mut delay, ref mut err) => {
                    if let Poll::Ready(res) = self.framed.close(cx) {
                        return match err.take() {
                            Some(Ok(_)) | None => Poll::Ready(
                                res.map_err(|e| DispatcherError::Encoder(e.into())),
                            ),
                            Some(Err(e)) => Poll::Ready(Err(e)),
                        };
                    } else {
                        ready!(Pin::new(delay).poll(cx));
                        return Poll::Ready(Ok(()));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};
    use derive_more::Display;
    use futures::future::ok;
    use std::io;

    use super::*;
    use crate::channel::mpsc;
    use crate::codec::{BytesCodec, Framed};
    use crate::rt::time::delay_for;
    use crate::testing::Io;

    #[test]
    fn test_err() {
        #[derive(Debug, Display)]
        struct TestError;
        type T = DispatcherError<TestError, BytesCodec>;
        let err = T::Encoder(io::Error::new(io::ErrorKind::Other, "err"));
        assert!(format!("{:?}", err).contains("DispatcherError::Encoder"));
        assert!(format!("{}", err).contains("Custom"));
        let err = T::Decoder(io::Error::new(io::ErrorKind::Other, "err"));
        assert!(format!("{:?}", err).contains("DispatcherError::Decoder"));
        assert!(format!("{}", err).contains("Custom"));
        let err = T::from(TestError);
        assert!(format!("{:?}", err).contains("DispatcherError::Service"));
        assert_eq!(format!("{}", err), "TestError");
    }

    #[ntex_rt::test]
    async fn test_basic() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let framed = Framed::new(server, BytesCodec);
        let disp = Dispatcher::new(
            framed,
            crate::fn_service(|msg: BytesMut| ok::<_, ()>(Some(msg.freeze()))),
        );
        crate::rt::spawn(disp.map(|_| ()));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[ntex_rt::test]
    async fn test_sink() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (tx, rx) = mpsc::channel();
        let framed = Framed::new(server, BytesCodec);
        let disp = Dispatcher::with(
            framed,
            Some(rx),
            crate::fn_service(|msg: BytesMut| ok::<_, ()>(Some(msg.freeze()))),
        )
        .disconnect_timeout(25);
        crate::rt::spawn(disp.map(|_| ()));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        assert!(tx.send(Bytes::from_static(b"test")).is_ok());
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        drop(tx);
        delay_for(Duration::from_millis(100)).await;
        assert!(client.is_server_dropped());
    }
}
