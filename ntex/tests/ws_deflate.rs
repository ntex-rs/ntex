//! `permessage-deflate` negotiation and compressed messages of the WebSocket client
//! and server.
#![cfg(all(feature = "ws", feature = "compress"))]
use std::sync::{Arc, Mutex, mpsc};
use std::{io, time::Duration};

use ntex::codec::{BytesCodec, Decoder, Encoder};
use ntex::http::test::{TestServer, server as test_server};
use ntex::http::{HttpService, Response, body::BodySize, h1, header};
use ntex::io::{DispatchItem, Dispatcher};
use ntex::service::Pipeline;
use ntex::util::{BytePages, ByteString, Bytes, BytesMut};
use ntex::web::{self, App, HttpRequest};
use ntex::ws::{self, DeflateConfig, WsClient, WsClientConfig, error::WsClientError};
use ntex::{SharedCfg, time::Seconds, time::timeout};

/// Server side codec, records the first byte of each received frame.
#[derive(Clone)]
struct Spy {
    codec: ws::Codec,
    received: Arc<Mutex<Vec<u8>>>,
}

impl Encoder for Spy {
    type Item = ws::Message;
    type Error = ws::error::ProtocolError;

    fn encode(&self, item: ws::Message, dst: &mut BytePages) -> Result<(), Self::Error> {
        self.codec.encode(item, dst)
    }
}

impl Decoder for Spy {
    type Item = ws::Frame;
    type Error = ws::error::ProtocolError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<ws::Frame>, Self::Error> {
        let first = src.first().copied();
        let frame = self.codec.decode(src)?;
        if let (Some(first), Some(_)) = (first, &frame) {
            self.received.lock().unwrap().push(first);
        }
        Ok(frame)
    }
}

/// Echoes data messages, answers pings and close frames.
async fn echo(msg: DispatchItem<Spy>) -> Result<Option<ws::Message>, io::Error> {
    Ok(match msg {
        DispatchItem::Item(frame) => Some(match frame {
            ws::Frame::Text(text) => ws::Message::Text(ByteString::try_from(text).unwrap()),
            ws::Frame::Binary(bin) => ws::Message::Binary(bin),
            ws::Frame::Continuation(item) => ws::Message::Continuation(item),
            ws::Frame::Ping(msg) => ws::Message::Pong(msg),
            ws::Frame::Pong(_) => return Ok(None),
            ws::Frame::Close(reason) => ws::Message::Close(reason),
        }),
        _ => None,
    })
}

struct Server {
    srv: TestServer,
    /// Extension offers of the clients.
    offers: mpsc::Receiver<Option<String>>,
    /// First bytes of the frames received by the server.
    received: Arc<Mutex<Vec<u8>>>,
}

impl Server {
    fn client(&self, cfg: WsClientConfig) -> WsClient<ntex::io::Base> {
        WsClient::new(
            self.srv.url("/"),
            cfg.set_address(self.srv.addr())
                .set_handshake_timeout(Seconds(30)),
        )
    }

    fn offer(&self) -> Option<String> {
        self.offers.recv_timeout(Duration::from_secs(3)).unwrap()
    }

    /// Returns the RSV1 bits of the frames received by the server.
    fn rsv1(&self) -> Vec<bool> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .map(|b| b & 0x40 != 0)
            .collect()
    }
}

/// Starts an echo server that accepts the extension with `response`, and
/// uses the `server` parameters for its codec.
fn deflate_server(response: Option<&'static str>, server: Option<DeflateConfig>) -> Server {
    let (offers_tx, offers) = mpsc::channel();
    let received = Arc::new(Mutex::new(Vec::new()));
    let received2 = received.clone();

    let srv = test_server(async move |_| {
        let offers_tx = offers_tx.clone();
        let received = received2.clone();
        let server = server.clone();
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();
                    let offer = req
                        .headers()
                        .get(header::SEC_WEBSOCKET_EXTENSIONS)
                        .map(|v| v.to_str().unwrap().to_owned());
                    let _ = offers_tx.send(offer);
                    let (server, received) = (server.clone(), received.clone());

                    let mut res = ws::handshake_response(req.head());
                    if let Some(response) = response {
                        res.header(header::SEC_WEBSOCKET_EXTENSIONS, response);
                    }
                    io.encode(
                        h1::Message::Item((res.build().drop_body(), BodySize::None)),
                        &codec,
                    )
                    .unwrap();

                    let mut codec = ws::Codec::new().max_size(4 * 1024 * 1024);
                    if let Some(cfg) = server {
                        codec = codec.set_deflate(cfg);
                    }
                    let spy = Spy { codec, received };
                    let _ = Dispatcher::new(io.seal(), spy, Pipeline::new((), echo)).await;
                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    Server {
        srv,
        offers,
        received,
    }
}

fn compressible(len: usize) -> Bytes {
    let text = b"The quick brown fox jumps over the lazy dog. ";
    text.iter()
        .copied()
        .cycle()
        .take(len)
        .collect::<Vec<_>>()
        .into()
}

fn incompressible(len: usize) -> Bytes {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect::<Vec<_>>()
        .into()
}

async fn roundtrip<F: ntex::io::Filter>(conn: ws::WsConnection<F>) {
    let (io, codec, _) = conn.into_inner();
    let large = compressible(1024 * 1024);
    let random = incompressible(200_000);

    let messages = [
        (
            ws::Message::Text("Hello".into()),
            ws::Frame::Text("Hello".into()),
        ),
        (
            ws::Message::Text("Hello".into()),
            ws::Frame::Text("Hello".into()),
        ),
        (
            ws::Message::Text(ByteString::new()),
            ws::Frame::Text(Bytes::new()),
        ),
        (
            ws::Message::Binary(large.clone()),
            ws::Frame::Binary(large.clone()),
        ),
        (
            ws::Message::Ping("ping".into()),
            ws::Frame::Pong("ping".into()),
        ),
        (
            ws::Message::Binary(random.clone()),
            ws::Frame::Binary(random.clone()),
        ),
        (
            ws::Message::Continuation(ws::Item::FirstText("Hello, ".into())),
            ws::Frame::Continuation(ws::Item::FirstText("Hello, ".into())),
        ),
        (
            ws::Message::Continuation(ws::Item::Continue(Bytes::new())),
            ws::Frame::Continuation(ws::Item::Continue(Bytes::new())),
        ),
        (
            ws::Message::Continuation(ws::Item::Last("world".into())),
            ws::Frame::Continuation(ws::Item::Last("world".into())),
        ),
        (
            ws::Message::Continuation(ws::Item::FirstBinary(random.slice(..1000))),
            ws::Frame::Continuation(ws::Item::FirstBinary(random.slice(..1000))),
        ),
        (
            ws::Message::Continuation(ws::Item::Last(Bytes::new())),
            ws::Frame::Continuation(ws::Item::Last(Bytes::new())),
        ),
        (
            ws::Message::Binary(Bytes::from_static(b"end")),
            ws::Frame::Binary(Bytes::from_static(b"end")),
        ),
    ];
    for (msg, expected) in messages {
        io.send(msg, &codec).await.unwrap();
        let frame = timeout(Seconds(5), io.recv(&codec))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(frame, expected);
    }

    io.send(
        ws::Message::Close(Some(ws::CloseCode::Normal.into())),
        &codec,
    )
    .await
    .unwrap();
    let frame = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(frame, ws::Frame::Close(Some(ws::CloseCode::Normal.into())));
}

/// RSV1 bits of the frames that `roundtrip` sends.
const ROUNDTRIP_RSV1: [bool; 13] = [
    true, true, true, true, false, true, true, false, false, true, false, true, false,
];

/// RSV1 bits of the frames that `roundtrip` sends without context takeover,
/// small unfragmented messages are not compressed.
const ROUNDTRIP_RSV1_NO_CONTEXT: [bool; 13] = [
    false, false, false, true, false, true, true, false, false, true, false, false, false,
];

fn client_cfg(deflate: DeflateConfig) -> WsClientConfig {
    WsClientConfig::new()
        .set_max_frame_size(4 * 1024 * 1024)
        .set_deflate(deflate)
}

/// STATEMENT: Negotiated `permessage-deflate` compresses data frames while leaving control and continuation frames correctly marked.
///
/// HOW:
/// - Assert the default extension offer, response header, and negotiated codec configuration.
/// - Run `roundtrip` over repeated/empty text, large compressible and random binary data, fragmented messages, ping/pong, and close; compare frames and `ROUNDTRIP_RSV1`.
#[ntex::test]
async fn deflate_accepted() {
    let srv = deflate_server(Some("permessage-deflate"), Some(DeflateConfig::new()));
    let conn = srv
        .client(client_cfg(DeflateConfig::new()))
        .connect()
        .await
        .unwrap();
    assert_eq!(
        srv.offer().as_deref(),
        Some("permessage-deflate; client_max_window_bits")
    );
    assert_eq!(conn.codec().deflate(), Some(&DeflateConfig::new()));
    assert_eq!(
        conn.response()
            .headers()
            .get(header::SEC_WEBSOCKET_EXTENSIONS)
            .unwrap(),
        "permessage-deflate"
    );

    roundtrip(conn).await;
    assert_eq!(srv.rsv1(), ROUNDTRIP_RSV1);
}

/// STATEMENT: Deflate negotiation applies agreed window/context limits while retaining the client's compression level.
///
/// HOW:
/// - Offer custom parameters and assert the wire offer and negotiated `DeflateConfig` use the agreed limits and level `1`.
/// - Run `roundtrip` and assert `ROUNDTRIP_RSV1_NO_CONTEXT`, including uncompressed small unfragmented messages.
#[ntex::test]
async fn deflate_accepted_with_params() {
    let agreed = DeflateConfig::new()
        .set_client_no_context_takeover(true)
        .set_server_no_context_takeover(true)
        .set_client_max_window_bits(9)
        .set_server_max_window_bits(10);
    let srv = deflate_server(
        Some(
            "permessage-deflate; client_no_context_takeover; server_no_context_takeover; \
             client_max_window_bits=9; server_max_window_bits=10",
        ),
        Some(agreed.clone()),
    );
    let offer = DeflateConfig::new()
        .set_compression_level(1)
        .set_server_no_context_takeover(true)
        .set_server_max_window_bits(12);
    let conn = srv.client(client_cfg(offer)).connect().await.unwrap();
    assert_eq!(
        srv.offer().as_deref(),
        Some(
            "permessage-deflate; server_no_context_takeover; client_max_window_bits; \
             server_max_window_bits=12"
        )
    );
    assert_eq!(
        conn.codec().deflate(),
        Some(&agreed.set_compression_level(1))
    );

    roundtrip(conn).await;
    assert_eq!(srv.rsv1(), ROUNDTRIP_RSV1_NO_CONTEXT);
}

/// STATEMENT: An agreed eight-bit client compression window supports complete message round trips.
///
/// HOW:
/// - Have the server select `client_max_window_bits=8` and assert the negotiated codec configuration.
/// - Run `roundtrip` to verify text, binary, fragmentation, control frames, and close with that window.
#[ntex::test]
async fn deflate_8_bit_client_window() {
    let agreed = DeflateConfig::new().set_client_max_window_bits(8);
    let srv = deflate_server(
        Some("permessage-deflate; client_max_window_bits=8"),
        Some(agreed.clone()),
    );
    let conn = srv
        .client(client_cfg(DeflateConfig::new()))
        .connect()
        .await
        .unwrap();
    assert_eq!(conn.codec().deflate(), Some(&agreed));
    roundtrip(conn).await;
}

/// STATEMENT: A declined deflate offer leaves messages uncompressed and fully functional.
///
/// HOW:
/// - Offer deflate to a server returning no extension and assert the offer exists but the codec has no deflate configuration.
/// - Run `roundtrip` and assert every recorded RSV1 bit is clear.
#[ntex::test]
async fn deflate_declined() {
    let srv = deflate_server(None, None);
    let conn = srv
        .client(client_cfg(DeflateConfig::new()))
        .connect()
        .await
        .unwrap();
    assert!(srv.offer().is_some());
    assert_eq!(conn.codec().deflate(), None);

    roundtrip(conn).await;
    assert!(srv.rsv1().iter().all(|rsv1| !rsv1));
}

/// STATEMENT: The client neither offers deflate by default nor accepts unsolicited extension negotiation.
///
/// HOW:
/// - Connect without deflate configuration and assert no offer and no codec deflate state.
/// - Have another server return unsolicited `permessage-deflate` and assert `UnexpectedWebSocketExtensions` contains that value.
#[ntex::test]
async fn deflate_not_offered() {
    let srv = deflate_server(None, None);
    let conn = srv.client(WsClientConfig::new()).connect().await.unwrap();
    assert_eq!(srv.offer(), None);
    assert_eq!(conn.codec().deflate(), None);

    // the server accepts an extension that is not offered
    let srv = deflate_server(Some("permessage-deflate"), None);
    let err = srv
        .client(WsClientConfig::new())
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(
        err.into_error(),
        WsClientError::UnexpectedWebSocketExtensions(val) if val == "permessage-deflate"
    ));
}

/// STATEMENT: Invalid, duplicate, incompatible, or unexpected WebSocket extension responses are rejected.
///
/// HOW:
/// - Try missing/out-of-range parameters, unknown parameters, duplicate offers, and an unmet no-context-takeover requirement; assert `InvalidWebSocketExtensions`.
/// - Return an unrelated extension and assert `UnexpectedWebSocketExtensions`.
#[ntex::test]
async fn deflate_invalid_response() {
    for response in [
        "permessage-deflate; client_max_window_bits",
        "permessage-deflate; server_max_window_bits=16",
        "permessage-deflate; unknown_param",
        "permessage-deflate, permessage-deflate",
    ] {
        let srv = deflate_server(Some(response), None);
        let err = srv
            .client(client_cfg(DeflateConfig::new()))
            .connect()
            .await
            .unwrap_err();
        assert!(
            matches!(
                err.into_error(),
                WsClientError::InvalidWebSocketExtensions(ref val) if val == response
            ),
            "{response}"
        );
    }

    // the server does not accept the requested limits
    let srv = deflate_server(Some("permessage-deflate"), None);
    let err = srv
        .client(client_cfg(
            DeflateConfig::new().set_server_no_context_takeover(true),
        ))
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(
        err.into_error(),
        WsClientError::InvalidWebSocketExtensions(_)
    ));

    let srv = deflate_server(Some("x-webkit-deflate-frame"), None);
    let err = srv
        .client(client_cfg(DeflateConfig::new()))
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(
        err.into_error(),
        WsClientError::UnexpectedWebSocketExtensions(_)
    ));
}

/// STATEMENT: The split WebSocket sink and receiver share negotiated deflate across repeated messages.
///
/// HOW:
/// - Negotiate deflate and exchange 20 alternating large binary and text messages through the sink and receiver, asserting each frame.
/// - Assert an empty close reply and RSV1 set on all 20 data frames.
#[ntex::test]
async fn deflate_sink_and_receiver() {
    let srv = deflate_server(Some("permessage-deflate"), Some(DeflateConfig::new()));
    let conn = srv
        .client(client_cfg(DeflateConfig::new()))
        .connect()
        .await
        .unwrap()
        .seal();
    let sink = conn.sink();
    let rx = conn.receiver();

    let large = compressible(1024 * 1024);
    for i in 0..20 {
        let msg = if i % 2 == 0 {
            ws::Message::Binary(large.slice(i..))
        } else {
            ws::Message::Text(format!("message {i}").into())
        };
        sink.send(msg).await.unwrap();
        let frame = timeout(Seconds(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let expected = if i % 2 == 0 {
            ws::Frame::Binary(large.slice(i..))
        } else {
            ws::Frame::Text(format!("message {i}").into())
        };
        assert_eq!(frame, expected);
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
    let frame = timeout(Seconds(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame, ws::Frame::Close(None));
    assert!(srv.rsv1().iter().take(20).all(|rsv1| *rsv1));
}

/// STATEMENT: A WebSocket byte transport retains negotiated compression.
///
/// HOW:
/// - Negotiate deflate, convert with `into_transport`, and send 100,000 compressible bytes using `BytesCodec`.
/// - Collect the echoed chunks, assert exact bytes, and assert RSV1 on every recorded data frame.
#[ntex::test]
async fn deflate_transport() {
    let srv = deflate_server(Some("permessage-deflate"), Some(DeflateConfig::new()));
    let conn = srv
        .client(client_cfg(DeflateConfig::new()))
        .connect()
        .await
        .unwrap();
    let io = conn.into_transport();

    let data = compressible(100_000);
    io.send(data.clone(), &BytesCodec).await.unwrap();
    let mut received = BytesMut::new();
    while received.len() < data.len() {
        let chunk = timeout(Seconds(5), io.recv(&BytesCodec))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        received.extend_from_slice(&chunk);
    }
    assert_eq!(received, data);
    assert!(srv.rsv1().iter().all(|rsv1| *rsv1));
}

/// STATEMENT: The WebSocket frame-size limit applies to decompressed data and triggers an uncompressed close.
///
/// HOW:
/// - Set a 64 KiB receive limit, negotiate deflate, and send a highly compressible message one byte larger than the limit.
/// - Assert `ProtocolError::Overflow`, receiver EOF, and recorded RSV1 bits for compressed data followed by an uncompressed close opcode.
#[ntex::test]
async fn decompressed_size_limit() {
    // the server sends a message that is larger than the client's limit once
    // decompressed
    let srv = deflate_server(Some("permessage-deflate"), Some(DeflateConfig::new()));
    let conn = srv
        .client(
            WsClientConfig::new()
                .set_max_frame_size(64 * 1024)
                .set_deflate(DeflateConfig::new()),
        )
        .connect()
        .await
        .unwrap()
        .seal();
    let sink = conn.sink();
    let rx = conn.receiver();

    // the echo of a small compressed frame
    sink.send(ws::Message::Binary(Bytes::from(vec![0; 64 * 1024 + 1])))
        .await
        .unwrap();
    let item = timeout(Seconds(5), rx.recv()).await.unwrap();
    assert!(
        matches!(
            item,
            Some(Err(ws::error::WsError::Protocol(
                ws::error::ProtocolError::Overflow
            )))
        ),
        "{item:?}"
    );
    assert!(rx.recv().await.is_none());

    // the client fails the connection with a protocol error
    timeout(Seconds(5), async {
        while srv.received.lock().unwrap().len() < 2 {
            ntex::time::sleep(ntex::time::Millis(10)).await;
        }
    })
    .await
    .expect("the server did not receive the close frame");
    assert_eq!(srv.rsv1(), [true, false]);
    let received = srv.received.lock().unwrap().clone();
    assert_eq!(received[1], 0x88);
}

/// STATEMENT: Compressed WebSocket control frames are rejected even after deflate negotiation.
///
/// HOW:
/// - Negotiate deflate and have a raw server send a control frame with RSV1 set.
/// - Assert the receiver reports `ProtocolError::ReservedBits(0b100)` within the timeout.
#[ntex::test]
async fn compressed_close_reason_is_rejected() {
    // control frames cannot be compressed, the client fails the connection
    let srv = test_server(async |_| {
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();
                    let res = ws::handshake_response(req.head())
                        .header(header::SEC_WEBSOCKET_EXTENSIONS, "permessage-deflate")
                        .build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();
                    io.send(Bytes::from_static(&[0xc9, 0x00]), &BytesCodec)
                        .await
                        .unwrap();
                    let codec = ws::Codec::new().set_deflate(DeflateConfig::new());
                    while let Ok(Some(_)) = io.recv(&codec).await {}
                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    let rx = WsClient::new(
        srv.url("/"),
        client_cfg(DeflateConfig::new()).set_address(srv.addr()),
    )
    .connect()
    .await
    .unwrap()
    .seal()
    .receiver();
    let item = timeout(Seconds(5), rx.recv()).await.unwrap();
    assert!(
        matches!(
            item,
            Some(Err(ws::error::WsError::Protocol(
                ws::error::ProtocolError::ReservedBits(0b100)
            )))
        ),
        "{item:?}"
    );
}

/// Starts a web echo server, `deflate` is the server's extension configuration.
fn web_deflate_server(deflate: Option<DeflateConfig>) -> web::test::TestServer {
    let mut cfg = WsClientConfig::new();
    if let Some(deflate) = deflate {
        cfg = cfg.set_deflate(deflate);
    }
    web::test::server_with(
        web::test::config().server_cfg(SharedCfg::new("SRV").add(cfg)),
        async |_| {
            App::new().service(
                web::resource("/").route(web::to(async move |req: HttpRequest| {
                    let _ = web::ws::start(&req, None, async |frame| {
                        Ok::<_, io::Error>(match frame {
                            ws::Frame::Text(text) => {
                                Some(ws::Message::Text(ByteString::try_from(text).unwrap()))
                            }
                            ws::Frame::Binary(bin) => Some(ws::Message::Binary(bin)),
                            ws::Frame::Continuation(item) => Some(ws::Message::Continuation(item)),
                            _ => None,
                        })
                    })
                    .await;
                })),
            )
        },
    )
}

/// Exchanges messages with a web echo server, returns the RSV1 bits of the
/// frames received by the client.
async fn web_roundtrip<F: ntex::io::Filter>(conn: ws::WsConnection<F>) -> Vec<bool> {
    let (io, codec, _) = conn.into_inner();
    let received = Arc::new(Mutex::new(Vec::new()));
    let spy = Spy {
        codec,
        received: received.clone(),
    };
    let large = compressible(32 * 1024);

    let messages = [
        (
            ws::Message::Text("Hello".into()),
            ws::Frame::Text("Hello".into()),
        ),
        (
            ws::Message::Binary(large.clone()),
            ws::Frame::Binary(large.clone()),
        ),
        (
            ws::Message::Ping("ping".into()),
            ws::Frame::Pong("ping".into()),
        ),
        (
            ws::Message::Continuation(ws::Item::FirstText("Hello, ".into())),
            ws::Frame::Continuation(ws::Item::FirstText("Hello, ".into())),
        ),
        (
            ws::Message::Continuation(ws::Item::Last("world".into())),
            ws::Frame::Continuation(ws::Item::Last("world".into())),
        ),
    ];
    for (msg, expected) in messages {
        io.send(msg, &spy).await.unwrap();
        let frame = timeout(Seconds(5), io.recv(&spy))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(frame, expected);
    }

    io.send(ws::Message::Close(Some(ws::CloseCode::Normal.into())), &spy)
        .await
        .unwrap();
    let frame = io.recv(&spy).await.unwrap().unwrap();
    assert_eq!(frame, ws::Frame::Close(Some(ws::CloseCode::Normal.into())));

    received
        .lock()
        .unwrap()
        .iter()
        .map(|b| b & 0x40 != 0)
        .collect()
}

/// STATEMENT: The web WebSocket server accepts a deflate offer when the extension is configured.
///
/// HOW:
/// - Configure deflate in the server's `SharedCfg` and connect a client offering it.
/// - Assert the response header and negotiated codec, then exchange data, control, and fragmented messages and assert the server compresses data frames.
#[ntex::test]
async fn web_deflate_accepted() {
    let srv = web_deflate_server(Some(DeflateConfig::new()));
    let conn = WsClient::new(
        srv.url("/"),
        client_cfg(DeflateConfig::new()).set_address(srv.addr()),
    )
    .connect()
    .await
    .unwrap();
    assert_eq!(
        conn.response()
            .headers()
            .get(header::SEC_WEBSOCKET_EXTENSIONS)
            .unwrap(),
        "permessage-deflate"
    );
    assert_eq!(conn.codec().deflate(), Some(&DeflateConfig::new()));
    assert_eq!(
        web_roundtrip(conn).await,
        [true, true, false, true, false, false]
    );
}

/// STATEMENT: The web WebSocket server applies its own deflate limits to the agreed parameters.
///
/// HOW:
/// - Configure context takeover and window limits on the server and connect a default client.
/// - Assert the response header and negotiated codec, then exchange messages and assert small messages are sent uncompressed.
#[ntex::test]
async fn web_deflate_server_limits() {
    let srv = web_deflate_server(Some(
        DeflateConfig::new()
            .set_client_no_context_takeover(true)
            .set_server_no_context_takeover(true)
            .set_client_max_window_bits(9)
            .set_server_max_window_bits(10),
    ));
    let conn = WsClient::new(
        srv.url("/"),
        client_cfg(DeflateConfig::new()).set_address(srv.addr()),
    )
    .connect()
    .await
    .unwrap();
    assert_eq!(
        conn.response()
            .headers()
            .get(header::SEC_WEBSOCKET_EXTENSIONS)
            .unwrap(),
        "permessage-deflate; client_no_context_takeover; server_no_context_takeover; \
         client_max_window_bits=9; server_max_window_bits=10"
    );
    assert_eq!(
        conn.codec().deflate(),
        Some(
            &DeflateConfig::new()
                .set_client_no_context_takeover(true)
                .set_server_no_context_takeover(true)
                .set_client_max_window_bits(9)
                .set_server_max_window_bits(10)
        )
    );
    assert_eq!(
        web_roundtrip(conn).await,
        [false, true, false, true, false, false]
    );
}

/// STATEMENT: The web WebSocket server does not use deflate unless both sides enable it.
///
/// HOW:
/// - Connect a deflate-offering client to a server without the extension, and a plain client to a deflate server.
/// - Assert no extension response header, no negotiated codec deflate, and uncompressed frames.
#[ntex::test]
async fn web_deflate_declined() {
    let srv = web_deflate_server(None);
    let conn = WsClient::new(
        srv.url("/"),
        client_cfg(DeflateConfig::new()).set_address(srv.addr()),
    )
    .connect()
    .await
    .unwrap();
    assert!(
        conn.response()
            .headers()
            .get(header::SEC_WEBSOCKET_EXTENSIONS)
            .is_none()
    );
    assert_eq!(conn.codec().deflate(), None);
    assert!(web_roundtrip(conn).await.iter().all(|rsv1| !rsv1));

    let srv = web_deflate_server(Some(DeflateConfig::new()));
    let conn = WsClient::new(srv.url("/"), WsClientConfig::new().set_address(srv.addr()))
        .connect()
        .await
        .unwrap();
    assert_eq!(conn.codec().deflate(), None);
    assert!(web_roundtrip(conn).await.iter().all(|rsv1| !rsv1));
}
