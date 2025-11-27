//! WebSockets protocol support
use std::{fmt, rc::Rc};

use ntex_io::DispatcherConfig;

pub use crate::ws::{CloseCode, CloseReason, Frame, Message, WsSink};

use crate::http::{StatusCode, body::BodySize, h1};
use crate::service::{
    IntoServiceFactory, ServiceFactory, apply_fn, chain_factory, fn_factory_with_config,
};
use crate::web::{HttpRequest, HttpResponse};
use crate::ws::{self, error::HandshakeError, error::WsError, handshake};
use crate::{io::DispatchItem, rt, time::Seconds, util::Either, util::Ready};

// ================================================================================================
// Private Helper Function to Avoid Repetition
// ================================================================================================

/// Wraps a `Frame` service factory into a `DispatchItem` service factory.
/// This helper function contains the logic that was previously duplicated.
fn create_ws_service_factory<T, F>(
    factory: F,
) -> impl ServiceFactory<
    DispatchItem<ws::Codec>,
    WsSink,
    Response = Option<Message>,
    Error = WsError<T::Error>,
    InitError = T::InitError,
>
where
    T: ServiceFactory<Frame, WsSink, Response = Option<Message>> + 'static,
    T::Error: fmt::Debug,
    T::InitError: 'static,
    F: IntoServiceFactory<T, Frame, WsSink>,
{
    let inner_factory =
        Rc::new(chain_factory(factory.into_factory()).map_err(WsError::Service));

    fn_factory_with_config(move |sink: WsSink| {
        let factory = inner_factory.clone();

        async move {
            let srv = factory.create(sink.clone()).await?;
            let sink = sink.clone();

            // This service wrapper translates DispatchItem into Frame
            Ok::<_, T::InitError>(apply_fn(srv, move |req, srv| match req {
                DispatchItem::Item(item) => {
                    // Clone sink only on close frame
                    let s = if matches!(item, Frame::Close(_)) {
                        Some(sink.clone())
                    } else {
                        None
                    };
                    Either::Left(async move {
                        let result = srv.call(item).await;
                        // after service call, close connection if needed
                        if let Some(s) = s {
                            let _ = rt::spawn(async move { s.io().close() });
                        }
                        result
                    })
                }
                // These items are not passed to the user service
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
    })
}

// ================================================================================================
// Public API
// ================================================================================================

/// Do websocket handshake and start websockets service.
///
/// This is the simplest way to start a WebSocket service. It uses default dispatcher configurations.
pub async fn start<T, F, Err>(req: HttpRequest, factory: F) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<Frame, WsSink, Response = Option<Message>> + 'static,
    T::Error: fmt::Debug,
    T::InitError: 'static,
    F: IntoServiceFactory<T, Frame, WsSink> + 'static,
    Err: From<T::InitError> + From<HandshakeError>,
{
    let ws_factory = create_ws_service_factory(factory);
    start_service(req, ws_factory).await
}

/// Do websocket handshake and start websockets service with custom configuration.
///
/// Use this function when you need to provide a custom `DispatcherConfig`.
pub async fn start_with_config<T, F, Err>(
    req: HttpRequest,
    factory: F,
    cfg: DispatcherConfig,
) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<Frame, WsSink, Response = Option<Message>> + 'static,
    T::Error: fmt::Debug,
    T::InitError: 'static,
    F: IntoServiceFactory<T, Frame, WsSink> + 'static,
    Err: From<T::InitError> + From<HandshakeError>,
{
    let ws_factory = create_ws_service_factory(factory);
    start_service_with_config(req, ws_factory, cfg).await
}

/// Do websocket handshake and start websockets service (low-level).
///
/// This function is for advanced use cases where you work directly with a `DispatchItem` service.
pub async fn start_service<T, F, Err>(
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
    start_service_with_config(req, factory, DispatcherConfig::default()).await
}

/// Do websocket handshake and start websockets service with custom configuration (low-level).
///
/// This is the core function that performs the handshake and starts the dispatcher.
/// All other functions in this module delegate to this one.
pub async fn start_service_with_config<T, F, Err>(
    req: HttpRequest,
    factory: F,
    cfg: DispatcherConfig,
) -> Result<HttpResponse, Err>
where
    T: ServiceFactory<DispatchItem<ws::Codec>, WsSink, Response = Option<Message>>
        + 'static,
    T::Error: fmt::Debug,
    F: IntoServiceFactory<T, DispatchItem<ws::Codec>, WsSink>,
    Err: From<T::InitError> + From<HandshakeError>,
{
    log::trace!("Starting WebSocket handshake for {:?}", req.path());

    // Perform ws handshake
    let res = handshake(req.head())?.finish().into_parts().0;

    // Extract I/O and HTTP codec from request
    let (io, codec) = req
        .head()
        .take_io()
        .ok_or(HandshakeError::NoWebsocketUpgrade)?;

    // Send handshake response
    io.encode(h1::Message::Item((res, BodySize::Empty)), &codec)
        .map_err(|_| HandshakeError::NoWebsocketUpgrade)?;
    log::trace!("WebSocket handshake successful for {:?}", req.path());

    // Create WebSocket sink
    let codec = ws::Codec::new();
    let sink = WsSink::new(io.get_ref(), codec.clone());

    // Create the WebSocket service
    let srv = factory.into_factory().create(sink.clone()).await?;

    let cfg = crate::io::DispatcherConfig::default();
    cfg.set_keepalive_timeout(Seconds::ZERO);

    // start websockets service dispatcher
    let _ = rt::spawn(async move {
        let res = crate::io::Dispatcher::new(io, codec, srv, &cfg).await;
        log::trace!("Ws handler is terminated: {res:?}");
    });

    // Return an empty OK response, as the connection has been upgraded.
    Ok(HttpResponse::new(StatusCode::OK))
}
