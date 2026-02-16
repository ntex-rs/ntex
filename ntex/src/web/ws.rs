//! `WebSockets` protocol support
use std::{fmt, rc::Rc};

pub use crate::ws::{CloseCode, CloseReason, Frame, Message, WsSink};

use crate::http::{StatusCode, body::BodySize, h1, header};
use crate::io::{DispatchItem, IoConfig, Reason};
use crate::service::{
    IntoServiceFactory, Service, ServiceFactory, chain_factory, fn_factory_with_config,
};
use crate::web::{HttpRequest, HttpResponse};
use crate::ws::{self, error::HandshakeError, error::WsError, handshake};
use crate::{SharedCfg, rt, time::Seconds};

thread_local! {
    static CFG: SharedCfg = SharedCfg::new("WS")
        .add(IoConfig::new().set_keepalive_timeout(Seconds::ZERO))
        .into();
}

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
pub async fn start<T, F, P, Err>(
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

    let factory = fn_factory_with_config(async move |sink: WsSink| {
        let srv = inner_factory.pipeline(sink.clone()).await?;
        let sink = sink.clone();

        Ok::<_, T::InitError>(DispatchService { srv, sink })
    });

    start_with(req, subprotocol, factory).await
}

/// Start websocket service handling raw `DispatchItem` messages requiring manual control/stop logic,
/// including the chosen subprotocol in the response.
///
/// If `subprotocol` is `Some`, the `Sec-Websocket-Protocol` header will be included
/// in the response with the chosen protocol. If `None`, the header is omitted.
pub async fn start_with<T, F, P, Err>(
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
    io.set_config(CFG.with(|cfg| *cfg));

    // the h1 dispatcher may have started a headers-read timer on this IO;
    // cancel it so DSP_TIMEOUT doesn't fire on the new WS dispatcher
    io.stop_timer();

    // start websockets service dispatcher
    rt::spawn(async move {
        let res = crate::io::Dispatcher::new(io, codec, srv).await;
        log::trace!("Ws handler is terminated: {res:?}");
    });

    Ok(HttpResponse::new(StatusCode::OK))
}

/// Just a wrapper over a service handling WebSocket messages and propagating shutdown
struct DispatchService<S> {
    srv: crate::service::Pipeline<S>,
    sink: WsSink,
}

impl<S, E> Service<DispatchItem<ws::Codec>> for DispatchService<S>
where
    S: crate::service::Service<Frame, Response = Option<Message>, Error = WsError<E>>,
    E: fmt::Debug,
{
    type Response = Option<Message>;
    type Error = WsError<E>;

    async fn call(
        &self,
        req: DispatchItem<ws::Codec>,
        _: crate::service::ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        match req {
            DispatchItem::Item(item) => {
                let s = if matches!(item, Frame::Close(_)) {
                    Some(self.sink.clone())
                } else {
                    None
                };
                let result = self.srv.call(item).await;
                if let Some(s) = s {
                    rt::spawn(async move { s.io().close() });
                }
                result
            }
            DispatchItem::Control(_) => Ok(None),
            DispatchItem::Stop(Reason::KeepAliveTimeout) => Err(WsError::KeepAlive),
            DispatchItem::Stop(Reason::ReadTimeout) => Err(WsError::ReadTimeout),
            DispatchItem::Stop(Reason::Decoder(e) | Reason::Encoder(e)) => {
                Err(WsError::Protocol(e))
            }
            DispatchItem::Stop(Reason::Io(e)) => Err(WsError::Disconnected(e)),
        }
    }

    async fn shutdown(&self) {
        // propagate shutdown to the user service
        self.srv.shutdown().await;
    }
}
