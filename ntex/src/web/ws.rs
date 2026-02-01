//! WebSockets protocol support
use std::{fmt, rc::Rc};

pub use crate::ws::{CloseCode, CloseReason, Frame, Message, WsSink};

use crate::http::{body::BodySize, h1, header, StatusCode};
use crate::service::{
    apply_fn, chain_factory, fn_factory_with_config, IntoServiceFactory, ServiceFactory,
};
use crate::web::{HttpRequest, HttpResponse};
use crate::ws::{self, error::HandshakeError, error::WsError, handshake};
use crate::{io::DispatchItem, rt, time::Seconds, util::Either, util::Ready};

/// Returns an iterator over the subprotocols requested by the client
/// in the `Sec-Websocket-Protocol` header.
///
/// # Example
///
/// ```ignore
/// use ntex::web::{self, HttpRequest, HttpResponse};
/// use ntex::web::ws;
///
/// async fn handler(req: HttpRequest) -> Result<HttpResponse, web::Error> {
///     // Note: convert to owned String since `req` will be moved
///     let chosen: Option<String> = ws::subprotocols(&req)
///         .find(|p| *p == "my-subprotocol")
///         .map(String::from);
///
///     ws::start_using_subprotocol(req, chosen, factory).await
/// }
/// ```
pub fn subprotocols(req: &HttpRequest) -> impl Iterator<Item = &str> {
    req.headers()
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .flat_map(|val| {
            val.to_str()
                .ok()
                .into_iter()
                .flat_map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()))
        })
}

/// Start websocket service handling Frame messages with automatic control/stop logic.
pub async fn start<T, F, Err>(req: HttpRequest, factory: F) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<Frame, WsSink, Response = Option<Message>> + 'static,
    T::Error: fmt::Debug,
    F: IntoServiceFactory<T, Frame, WsSink>,
    Err: From<T::InitError> + From<HandshakeError>,
{
    start_using_subprotocol(req, None::<&str>, factory).await
}

/// Start websocket service handling Frame messages with automatic control/stop logic,
/// including the chosen subprotocol in the response.
///
/// If `subprotocol` is `Some`, the `Sec-Websocket-Protocol` header will be included
/// in the response with the chosen protocol. If `None`, the header is omitted.
///
/// # Example
///
/// ```ignore
/// use ntex::web::{self, HttpRequest, HttpResponse};
/// use ntex::web::ws;
///
/// async fn handler(req: HttpRequest) -> Result<HttpResponse, web::Error> {
///     // Note: convert to owned String since `req` will be moved
///     let chosen: Option<String> = ws::subprotocols(&req)
///         .find(|p| *p == "graphql-ws" || *p == "graphql-transport-ws")
///         .map(String::from);
///
///     ws::start_using_subprotocol(req, chosen, factory).await
/// }
/// ```
pub async fn start_using_subprotocol<T, F, P, Err>(
    req: HttpRequest,
    subprotocol: Option<P>,
    factory: F,
) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<Frame, WsSink, Response = Option<Message>> + 'static,
    T::Error: fmt::Debug,
    F: IntoServiceFactory<T, Frame, WsSink>,
    P: AsRef<str>,
    Err: From<T::InitError> + From<HandshakeError>,
{
    let inner_factory = Rc::new(chain_factory(factory).map_err(WsError::Service));

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
                            let _ = rt::spawn(async move { s.io().close() });
                        }
                        result
                    })
                }
                DispatchItem::WBackPressureEnabled
                | DispatchItem::WBackPressureDisabled => Either::Right(Ready::Ok(None)),
                DispatchItem::KeepAliveTimeout => {
                    Either::Right(Ready::Err(WsError::KeepAlive))
                }
                DispatchItem::ReadTimeout => {
                    Either::Right(Ready::Err(WsError::ReadTimeout))
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

    start_using_subprotocol_with(req, subprotocol, factory).await
}

/// Start websocket service handling raw DispatchItem messages requiring manual control/stop logic.
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
    start_using_subprotocol_with(req, None::<&str>, factory).await
}

/// Start websocket service handling raw DispatchItem messages requiring manual control/stop logic,
/// including the chosen subprotocol in the response.
///
/// If `subprotocol` is `Some`, the `Sec-Websocket-Protocol` header will be included
/// in the response with the chosen protocol. If `None`, the header is omitted.
pub async fn start_using_subprotocol_with<T, F, P, Err>(
    req: HttpRequest,
    subprotocol: Option<P>,
    factory: F,
) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<DispatchItem<ws::Codec>, WsSink, Response = Option<Message>>
        + 'static,
    T::Error: fmt::Debug,
    F: IntoServiceFactory<T, DispatchItem<ws::Codec>, WsSink>,
    P: AsRef<str>,
    Err: From<T::InitError> + From<HandshakeError>,
{
    log::trace!("Start ws handshake verification for {:?}", req.path());

    // ws handshake
    let mut res = handshake(req.head())?;
    if let Some(protocol) = subprotocol {
        res.set_header(header::SEC_WEBSOCKET_PROTOCOL, protocol.as_ref());
    }
    let res = res.finish().into_parts().0;

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

    let cfg = crate::io::DispatcherConfig::default();
    cfg.set_keepalive_timeout(Seconds::ZERO);

    // start websockets service dispatcher
    let _ = rt::spawn(async move {
        let res = crate::io::Dispatcher::new(io, codec, srv, &cfg).await;
        log::trace!("Ws handler is terminated: {res:?}");
    });

    Ok(HttpResponse::new(StatusCode::OK))
}
