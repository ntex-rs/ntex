use std::{
    error::Error as StdError, marker::PhantomData, pin::Pin, task::Context, task::Poll,
};

use bytes::Bytes;
use futures_core::Stream;
use futures_sink::Sink;

pub use crate::ws::{CloseCode, CloseReason, Frame, Message};

use crate::http::body::{Body, BoxedBodyStream};
use crate::http::error::PayloadError;
use crate::http::ws::{handshake, HandshakeError};
use crate::service::{IntoServiceFactory, Service, ServiceFactory};
use crate::web::{HttpRequest, HttpResponse};
use crate::{channel::mpsc, rt, ws};

pub type WebSocketsSink =
    ws::StreamEncoder<mpsc::Sender<Result<Bytes, Box<dyn StdError>>>>;

/// Do websocket handshake and start websockets service.
pub async fn start<T, F, S, Err>(
    req: HttpRequest,
    payload: S,
    factory: F,
) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<
        Config = WebSocketsSink,
        Request = Frame,
        Response = Option<Message>,
    >,
    T::Error: StdError + 'static,
    T::InitError: 'static,
    T::Service: 'static,
    F: IntoServiceFactory<T>,
    S: Stream<Item = Result<Bytes, PayloadError>> + Unpin + 'static,
    Err: From<T::InitError>,
    Err: From<HandshakeError>,
{
    let (tx, rx) = mpsc::channel();

    start_with(req, payload, tx, rx, factory).await
}

/// Do websocket handshake and start websockets service.
pub async fn start_with<T, F, S, Err, Tx, Rx>(
    req: HttpRequest,
    payload: S,
    tx: Tx,
    rx: Rx,
    factory: F,
) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<
        Config = ws::StreamEncoder<Tx>,
        Request = Frame,
        Response = Option<Message>,
    >,
    T::Error: StdError + 'static,
    T::InitError: 'static,
    T::Service: 'static,
    F: IntoServiceFactory<T>,
    S: Stream<Item = Result<Bytes, PayloadError>> + Unpin + 'static,
    Err: From<T::InitError>,
    Err: From<HandshakeError>,
    Tx: Sink<Result<Bytes, Box<dyn StdError>>> + Clone + Unpin + 'static,
    Tx::Error: StdError,
    Rx: Stream<Item = Result<Bytes, Box<dyn StdError>>> + Unpin + 'static,
{
    // ws handshake
    let mut res = handshake(req.head())?;

    // converter wraper from ws::Message to Bytes
    let sink = ws::StreamEncoder::new(tx);

    // create ws service
    let srv = factory
        .into_factory()
        .new_service(sink.clone())
        .await?
        .map_err(|e| {
            let e: Box<dyn StdError> = Box::new(e);
            e
        });

    // start websockets service dispatcher
    rt::spawn(crate::util::stream::Dispatcher::new(
        // wrap bytes stream to ws::Frame's stream
        MapStream {
            stream: ws::StreamDecoder::new(payload),
            _t: PhantomData,
        },
        // converter wraper from ws::Message to Bytes
        sink,
        // websockets handler service
        srv,
    ));

    Ok(res.body(Body::from_message(BoxedBodyStream::new(rx))))
}

pin_project_lite::pin_project! {
    struct MapStream<S, I, E>{
        #[pin]
        stream: S,
        _t: PhantomData<(I, E)>,
    }
}

impl<S, I, E> Stream for MapStream<S, I, E>
where
    S: Stream<Item = Result<I, E>>,
    E: StdError + 'static,
{
    type Item = Result<I, Box<dyn StdError>>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.project().stream.poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(item))) => Poll::Ready(Some(Ok(item))),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(Box::new(err)))),
            Poll::Ready(None) => Poll::Ready(None),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}
