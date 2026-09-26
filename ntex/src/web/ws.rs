//! `WebSockets` protocol support
use std::fmt;

pub use crate::ws::{CloseCode, CloseReason, Frame, Message, WsSink};

use crate::http::{body::BodySize, h1, header};
use crate::io::{DispatchItem, IoConfig, Reason};
use crate::service::{Ctx, IntoService, Pipeline, Service, apply_fn};
use crate::web::HttpRequest;
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
/// ```rust
/// use ntex::web::{self, HttpRequest, ws};
///
/// async fn service(frame: ws::Frame) -> Result<Option<ws::Message>, std::io::Error> {
///     // handle incoming frames
///     Ok(None)
/// }
///
/// async fn handler(req: HttpRequest) {
///     let chosen = ws::subprotocols(&req)
///         .find(|p| *p == "my-subprotocol");
///
///     if let Err(err) = ws::start(&req, chosen, service).await {
///         eprintln!("WebSocket error: {err:?}");
///     }
/// }
///
/// let app = web::App::default().route("/ws", web::get().to(handler));
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
/// in the response with the chosen protocol. The protocol must be a valid HTTP
/// token offered by the client. If `None`, the header is omitted.
///
/// # Example
///
/// ```rust
/// use ntex::web::{self, HttpRequest, ws};
///
/// async fn service(frame: ws::Frame) -> Result<Option<ws::Message>, std::io::Error> {
///     // handle incoming frames
///     Ok(None)
/// }
///
/// async fn handler(req: HttpRequest) {
///     let chosen = ws::subprotocols(&req)
///         .find(|p| *p == "graphql-ws" || *p == "graphql-transport-ws");
///
///     if let Err(err) = ws::start(&req, chosen, service).await {
///         eprintln!("WebSocket error: {err:?}");
///     }
/// }
///
/// let app = web::App::default().route("/ws", web::get().to(handler));
/// ```
pub async fn start<S>(
    req: &HttpRequest,
    subprotocol: Option<&str>,
    f: impl IntoService<S, WsSink, Frame>,
) -> Result<(), WsError<S::Error>>
where
    S: Service<WsSink, Frame, Res = Option<Message>> + 'static,
    S::Error: fmt::Debug,
{
    start_with(
        req,
        subprotocol,
        DispatchService {
            svc: f.into_service(),
        },
    )
    .await
}

/// Start websocket service handling raw `DispatchItem` messages requiring manual control/stop logic,
/// including the chosen subprotocol in the response.
///
/// If `subprotocol` is `Some`, the `Sec-Websocket-Protocol` header will be included
/// in the response with the chosen protocol. The protocol must be a valid HTTP
/// token offered by the client. If `None`, the header is omitted.
pub async fn start_with<S, Err>(
    req: &HttpRequest,
    subprotocol: Option<&str>,
    f: impl IntoService<S, WsSink, DispatchItem<WsSink>>,
) -> Result<(), WsError<Err>>
where
    S: Service<WsSink, DispatchItem<WsSink>, Res = Option<Message>, Error = WsError<Err>> + 'static,
    S::Error: fmt::Debug,
    Err: 'static,
{
    log::trace!("Start ws handshake verification for {:?}", req.path());

    // ws handshake
    let mut res = handshake(req.head())?;
    if let Some(protocol) = subprotocol {
        if !ws::is_token(protocol) || !subprotocols(req).any(|offered| offered == protocol) {
            return Err(HandshakeError::BadWebsocketProtocol.into());
        }
        res.set_header(header::SEC_WEBSOCKET_PROTOCOL, protocol);
    }
    let res = res.build().into_parts().0;

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

    // create sink, it is also the dispatcher's codec
    let sink = WsSink::new(io.get_ref(), ws::Codec::new(), io.shared().get());

    // create ws service
    // SAFETY: the HTTP dispatcher has transferred ownership of `io` to this
    // upgrade path, and no borrowed reference from `io.cfg()` is retained.
    unsafe {
        io.set_config(CFG.with(Clone::clone));
    }

    // the h1 dispatcher may have started a headers-read timer on this IO;
    // cancel it so DSP_TIMEOUT doesn't fire on the new WS dispatcher
    io.stop_timer();

    // start websockets service dispatcher
    let timeout_sink = sink.clone();
    let service = apply_fn(f.into_service(), async move |req, svc| {
        let result = svc.call(req).await;
        if matches!(&result, Ok(Some(Message::Close(_)))) {
            timeout_sink.start_close_timeout();
        }
        result
    });
    let result = crate::io::Dispatcher::new(io, sink.clone(), Pipeline::new(sink, service)).await;
    log::trace!("Ws handler is terminated: {result:?}");

    result
}

/// Just a wrapper over a service handling WebSocket messages and propagating shutdown
struct DispatchService<S> {
    svc: S,
}

impl<S, E> Service<WsSink, DispatchItem<WsSink>> for DispatchService<S>
where
    S: Service<WsSink, Frame, Res = Option<Message>, Error = E>,
    E: fmt::Debug,
{
    type Res = Option<Message>;
    type Error = WsError<E>;

    crate::forward_ready!(WsSink, svc, WsError::Service);
    crate::forward_shutdown!(WsSink, svc);

    async fn call(
        &self,
        req: DispatchItem<WsSink>,
        ctx: Ctx<'_, Self, WsSink>,
    ) -> Result<Self::Res, Self::Error> {
        match req {
            DispatchItem::Item(item) => {
                let s = if matches!(item, Frame::Close(_)) {
                    Some(ctx.st().clone())
                } else {
                    None
                };
                let result = ctx.call(&self.svc, item).await.map_err(WsError::Service);
                if let Some(s) = s {
                    rt::spawn(async move { s.io().close() });
                }
                result
            }
            // a clean disconnect is not an error
            DispatchItem::Control(_) | DispatchItem::Stop(Reason::Io(None)) => Ok(None),
            DispatchItem::Stop(Reason::Service) => {
                Ok(Some(Message::Close(Some(ws::CloseReason {
                    code: ws::CloseCode::Away,
                    description: None,
                }))))
            }
            DispatchItem::Stop(Reason::KeepAliveTimeout) => Err(WsError::KeepAlive),
            DispatchItem::Stop(Reason::ReadTimeout) => Err(WsError::ReadTimeout),
            DispatchItem::Stop(Reason::WriteTimeout) => Err(WsError::WriteTimeout),
            DispatchItem::Stop(Reason::Decoder(e) | Reason::Encoder(e)) => {
                Err(WsError::Protocol(e))
            }
            DispatchItem::Stop(Reason::Io(e)) => Err(WsError::Disconnected(e)),
        }
    }
}
