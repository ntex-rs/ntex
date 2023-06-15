//! WebSockets protocol support
use std::{fmt, rc::Rc};

pub use crate::ws::{CloseCode, CloseReason, Frame, Message, WsSink};

use crate::http::{body::BodySize, h1, StatusCode};
use crate::service::{
    apply_fn, fn_factory_with_config, IntoServiceFactory, ServiceFactory,
};
use crate::web::{HttpRequest, HttpResponse};
use crate::ws::{self, error::HandshakeError, error::WsError, handshake};
use crate::{io::DispatchItem, rt, time::Seconds, util::Either, util::Ready};

/// Do websocket handshake and start websockets service.
pub async fn start<T, F, Err>(req: HttpRequest, factory: F) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<Frame, WsSink, Response = Option<Message>> + 'static,
    T::Error: fmt::Debug,
    F: IntoServiceFactory<T, Frame, WsSink>,
    Err: From<T::InitError> + From<HandshakeError>,
{
    let inner_factory = Rc::new(factory.into_factory().map_err(WsError::Service));

    let factory = fn_factory_with_config(move |sink: WsSink| {
        let factory = inner_factory.clone();

        async move {
            let srv = factory.create(sink.clone()).await?;
            let sink = sink.clone();

            Ok::<_, T::InitError>(apply_fn(srv, move |req, srv| match req {
                DispatchItem::<ws::Codec>::Item(item) => {
                    let s = if matches!(item, Frame::Close(_)) {
                        Some(sink.clone())
                    } else {
                        None
                    };
                    Either::Left(async move {
                        let result = srv.call(item).await;
                        if let Some(s) = s {
                            rt::spawn(async move { s.io().close() });
                        }
                        result
                    })
                }
                DispatchItem::WBackPressureEnabled
                | DispatchItem::WBackPressureDisabled => Either::Right(Ready::Ok(None)),
                DispatchItem::KeepAliveTimeout => {
                    Either::Right(Ready::Err(WsError::KeepAlive))
                }
                DispatchItem::DecoderError(e) | DispatchItem::EncoderError(e) => {
                    Either::Right(Ready::Err(WsError::Protocol(e)))
                }
                DispatchItem::Disconnect(e) => {
                    Either::Right(Ready::Err(WsError::Disconnected(e)))
                }
            }))
        }
    });

    start_with(req, factory).await
}

/// Do websocket handshake and start websockets service.
pub async fn start_with<T, F, Err>(
    req: HttpRequest,
    factory: F,
) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<DispatchItem<ws::Codec>, WsSink, Response = Option<Message>>
        + 'static,
    T::Error: fmt::Debug,
    F: IntoServiceFactory<T, DispatchItem<ws::Codec>, WsSink>,
    Err: From<T::InitError> + From<HandshakeError>,
{
    log::trace!("Start ws handshake verification for {:?}", req.path());

    // ws handshake
    let res = handshake(req.head())?.finish().into_parts().0;

    // extract io
    let item = req
        .head()
        .take_io()
        .ok_or(HandshakeError::NoWebsocketUpgrade)?;
    let io = item.0;
    let codec = item.1;

    io.encode(h1::Message::Item((res, BodySize::Empty)), &codec)
        .map_err(|_| HandshakeError::NoWebsocketUpgrade)?;
    log::trace!("Ws handshake verification completed for {:?}", req.path());

    // create sink
    let codec = ws::Codec::new();
    let sink = WsSink::new(io.get_ref(), codec.clone());

    // create ws service
    let srv = factory.into_factory().create(sink.clone()).await?;

    // start websockets service dispatcher
    rt::spawn(async move {
        let res = crate::io::Dispatcher::new(io, codec, srv)
            .keepalive_timeout(Seconds::ZERO)
            .await;
        log::trace!("Ws handler is terminated: {:?}", res);
    });

    Ok(HttpResponse::new(StatusCode::OK))
}
