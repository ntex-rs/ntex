use std::{error, marker::PhantomData, pin::Pin, task::Context, task::Poll};

pub use crate::ws::{CloseCode, CloseReason, Frame, Message};

use crate::http::body::{Body, BoxedBodyStream};
use crate::http::error::PayloadError;
use crate::http::ws::{handshake, HandshakeError};
use crate::service::{IntoServiceFactory, Service, ServiceFactory};
use crate::web::{HttpRequest, HttpResponse};
use crate::{channel::mpsc, rt, util::Bytes, ws, Sink, Stream};

pub type WebSocketsSink =
    ws::StreamEncoder<mpsc::Sender<Result<Bytes, Box<dyn error::Error>>>>;

// TODO: fix close frame handling
/// Do websocket handshake and start websockets service.
pub async fn start<T, F, S, Err>(
    req: HttpRequest,
    payload: S,
    factory: F,
) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<Frame, WebSocketsSink, Response = Option<Message>> + 'static,
    T::Error: error::Error,
    F: IntoServiceFactory<T, Frame, WebSocketsSink>,
    S: Stream<Item = Result<Bytes, PayloadError>> + Unpin + 'static,
    Err: From<T::InitError> + From<HandshakeError>,
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
    T: ServiceFactory<Frame, ws::StreamEncoder<Tx>, Response = Option<Message>> + 'static,
    T::Error: error::Error,
    F: IntoServiceFactory<T, Frame, ws::StreamEncoder<Tx>>,
    S: Stream<Item = Result<Bytes, PayloadError>> + Unpin + 'static,
    Err: From<T::InitError> + From<HandshakeError>,
    Tx: Sink<Result<Bytes, Box<dyn error::Error>>> + Clone + Unpin + 'static,
    Tx::Error: error::Error,
    Rx: Stream<Item = Result<Bytes, Box<dyn error::Error>>> + Unpin + 'static,
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
            let e: Box<dyn error::Error> = Box::new(e);
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
    E: error::Error + 'static,
{
    type Item = Result<I, Box<dyn error::Error>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
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
