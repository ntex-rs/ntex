use std::error::Error as StdError;

use bytes::Bytes;
use futures::{Stream, TryStreamExt};

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
    factory: F,
    req: HttpRequest,
    payload: S,
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
    // ws handshake
    let mut res = handshake(req.head())?;

    let payload = payload.map_err(|e| {
        let e: Box<dyn StdError> = Box::new(e);
        e
    });

    // response body stream
    let (tx, rx): (_, mpsc::Receiver<Result<Bytes, Box<dyn StdError>>>) =
        mpsc::channel();
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

    // start websockets protocol dispatcher
    rt::spawn(crate::util::stream::Dispatcher::new(
        // wrap bytes stream to ws::Frame's stream
        ws::StreamDecoder::new(payload).map_err(|e| {
            let e: Box<dyn StdError> = Box::new(e);
            e
        }),
        // converter wraper from ws::Message to Bytes
        sink,
        // websockets handler service
        srv,
    ));

    Ok(res.body(Body::from_message(BoxedBodyStream::new(rx))))
}
