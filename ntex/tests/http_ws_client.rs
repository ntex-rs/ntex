use std::io;

use ntex::codec::BytesCodec;
use ntex::http::test::server as test_server;
use ntex::http::{HttpService, Response, body::BodySize, h1};
use ntex::io::{DispatchItem, Dispatcher, IoConfig};
use ntex::service::{Pipeline, cfg::SharedCfg};
use ntex::web::{self, App, HttpRequest};
use ntex::ws::{self, handshake_response};
use ntex::{time::Seconds, util::ByteString, util::Bytes};

async fn ws_service(msg: DispatchItem<ws::Codec>) -> Result<Option<ws::Message>, io::Error> {
    let msg = match msg {
        DispatchItem::Item(msg) => match msg {
            ws::Frame::Ping(msg) => ws::Message::Pong(msg),
            ws::Frame::Text(text) => {
                ws::Message::Text(String::from_utf8(Vec::from(text.as_ref())).unwrap().into())
            }
            ws::Frame::Binary(bin) => ws::Message::Binary(bin),
            ws::Frame::Close(reason) => ws::Message::Close(reason),
            _ => ws::Message::Close(None),
        },
        _ => return Ok(None),
    };
    Ok(Some(msg))
}

/// STATEMENT: The WebSocket client exchanges data and control frames through a dispatcher-backed server.
///
/// HOW:
/// - Upgrade into an echo dispatcher and send text, binary, ping, and Normal close messages.
/// - Assert matching text/binary echoes, a pong, and the Normal close reply.
#[ntex::test]
async fn test_simple() {
    let srv = test_server(async |_| {
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();

                    // send handshake respone
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    // start websocket service
                    let _ = Dispatcher::new(
                        io.seal(),
                        ws::Codec::default(),
                        Pipeline::new((), ws_service),
                    )
                    .await;
                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    // client service
    let (io, codec, _) = srv.ws().await.unwrap().into_inner();
    io.send(ws::Message::Text(ByteString::from_static("text")), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Text(Bytes::from_static(b"text")));

    io.send(ws::Message::Binary("text".into()), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    io.send(ws::Message::Ping("text".into()), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Pong("text".to_string().into()));

    io.send(
        ws::Message::Close(Some(ws::CloseCode::Normal.into())),
        &codec,
    )
    .await
    .unwrap();

    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Normal.into())));
}

/// STATEMENT: A client WebSocket transport exposes echoed binary messages as bytes.
///
/// HOW:
/// - Connect to an echo dispatcher and convert the connection with `into_transport`.
/// - Send `text` using `BytesCodec` and assert the received bytes match.
#[ntex::test]
async fn test_transport() {
    let srv = test_server(async |_| {
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();

                    // send handshake respone
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    // start websocket service
                    let _ = Dispatcher::new(
                        io.seal(),
                        ws::Codec::default(),
                        Pipeline::new((), ws_service),
                    )
                    .await;

                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    // client service
    let io = srv.ws().await.unwrap().into_transport();

    io.send(Bytes::from_static(b"text"), &BytesCodec)
        .await
        .unwrap();
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"text"));
}

/// STATEMENT: An idle WebSocket connection closes after the server's keepalive timeout.
///
/// HOW:
/// - Set the upgraded server I/O keepalive to one second and connect with a sink and receiver.
/// - Verify a binary echo, then assert the next receive returns `None` without more traffic.
#[ntex::test]
async fn test_keepalive_timeout() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::NotFound())).control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();

                    // send handshake respone
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    // start websocket service
                    // SAFETY: no reference returned by `io.cfg()` is retained
                    // across the protocol transition.
                    unsafe {
                        io.set_config(
                            SharedCfg::new("WS-SRV")
                                .add(IoConfig::new().set_keepalive_timeout(Seconds::ONE)),
                        );
                    }
                    let _ = Dispatcher::new(
                        io.seal(),
                        ws::Codec::default(),
                        Pipeline::new((), ws_service),
                    )
                    .await;

                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    // client service
    let con = ws::WsClient::new(
        srv.url("/"),
        ws::WsClientConfig::new()
            .set_address(srv.addr())
            .set_handshake_timeout(Seconds(30)),
    )
    .connect()
    .await
    .unwrap()
    .seal();
    let tx = con.sink();
    let rx = con.receiver();

    tx.send(ws::Message::Binary(Bytes::from_static(b"text")))
        .await
        .unwrap();
    let item = rx.recv().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    let item = rx.recv().await;
    assert!(item.is_none());
}

/// STATEMENT: A WebSocket handshake can complete after the web handler yields.
///
/// HOW:
/// - Have a web route await a zero-duration sleep before calling `web::ws::start`.
/// - Connect with a one-second handshake timeout and require successful negotiation.
#[ntex::test]
async fn test_upgrade_handler_with_await() {
    async fn ws_service(_: ws::Frame) -> Result<Option<ws::Message>, io::Error> {
        Ok(None)
    }

    let srv = test_server(async |_| {
        HttpService::new(App::new().service(web::resource("/").route(web::to(
            async move |req: HttpRequest| {
                // some async context switch
                ntex::time::sleep(ntex::time::Seconds::ZERO).await;
                let _ = web::ws::start(&req, None, ws_service).await;
            },
        ))))
    });

    let _ = ws::WsClient::new(
        srv.url("/"),
        ws::WsClientConfig::new()
            .set_address(srv.addr())
            .set_handshake_timeout(Seconds(1)),
    )
    .connect()
    .await
    .unwrap();
}

/// STATEMENT: Dropping an idle WebSocket receiver initiates a Normal closing handshake.
///
/// HOW:
/// - Connect to a server recording close frames, retain the sink, and confirm its I/O is active.
/// - Drop the receiver, await disconnect within three seconds, and assert the peer received a Normal close.
#[ntex::test]
async fn test_receiver_drop_closes_connection() {
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let srv = test_server(async move |_| {
        let closed_tx = closed_tx.clone();
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    let closed_tx = closed_tx.clone();
                    let _ = Dispatcher::new(
                        io.seal(),
                        ws::Codec::default(),
                        Pipeline::new((), async move |msg: DispatchItem<ws::Codec>| {
                            if let DispatchItem::Item(ws::Frame::Close(reason)) = &msg {
                                let _ = closed_tx.send(reason.clone());
                            }
                            ws_service(msg).await
                        }),
                    )
                    .await;
                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    let con = ws::WsClient::new(
        srv.url("/"),
        ws::WsClientConfig::new()
            .set_address(srv.addr())
            .set_handshake_timeout(Seconds(30)),
    )
    .connect()
    .await
    .unwrap()
    .seal();
    let sink = con.sink();
    let rx = con.receiver();
    ntex::time::sleep(ntex::time::Millis(50)).await;
    assert!(sink.io().is_active());

    // the peer is idle, dropping the receiver still closes the connection
    drop(rx);
    ntex::time::timeout(Seconds(3), sink.on_disconnect())
        .await
        .expect("connection is not closed");
    assert_eq!(
        closed_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        Some(ws::CloseCode::Normal.into())
    );
}

/// Starts a server that runs the peer side of the closing handshake and reports
/// the frames it receives until the client closes the connection.
fn close_handshake_server(
    server_first: bool,
) -> (
    ntex::http::test::TestServer,
    std::sync::mpsc::Receiver<Vec<ws::Frame>>,
) {
    let (frames_tx, frames_rx) = std::sync::mpsc::channel();
    let srv = test_server(async move |_| {
        let frames_tx = frames_tx.clone();
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    let codec = ws::Codec::default();
                    if server_first {
                        io.send(ws::Message::Close(Some(ws::CloseCode::Away.into())), &codec)
                            .await
                            .unwrap();
                    }
                    let mut frames = Vec::new();
                    while let Ok(Some(frame)) = io.recv(&codec).await {
                        if !server_first && matches!(frame, ws::Frame::Close(_)) {
                            let _ = io.send(ws::Message::Close(None), &codec).await;
                        }
                        frames.push(frame);
                    }
                    let _ = frames_tx.send(frames);
                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });
    (srv, frames_rx)
}

async fn close_handshake_client(
    srv: &ntex::http::test::TestServer,
) -> ws::WsConnection<ntex::io::Sealed> {
    ws::WsClient::new(
        srv.url("/"),
        ws::WsClientConfig::new()
            .set_address(srv.addr())
            .set_handshake_timeout(Seconds(30)),
    )
    .connect()
    .await
    .unwrap()
    .seal()
}

/// STATEMENT: A WebSocket receiver answers a peer-initiated close exactly once with the same code.
///
/// HOW:
/// - Use `close_handshake_server(true)` to send an Away close first.
/// - Assert the receiver yields that close then EOF, and the server records exactly one echoed Away close.
#[ntex::test]
async fn test_receiver_answers_peer_close() {
    let (srv, frames) = close_handshake_server(true);
    let rx = close_handshake_client(&srv).await.receiver();

    let item = rx.recv().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Away.into())));
    assert!(rx.recv().await.is_none());

    // the client echoed the close code
    assert_eq!(
        frames
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap(),
        vec![ws::Frame::Close(Some(ws::CloseCode::Away.into()))]
    );
}

/// STATEMENT: A WebSocket receiver does not answer the peer's reply to a locally initiated close.
///
/// HOW:
/// - Send a Normal close to `close_handshake_server(false)` and receive its empty close reply.
/// - Assert the receiver ends and the server recorded only the original Normal close, with no extra reply.
#[ntex::test]
async fn test_receiver_does_not_answer_close_reply() {
    let (srv, frames) = close_handshake_server(false);
    let con = close_handshake_client(&srv).await;
    let sink = con.sink();
    let rx = con.receiver();

    sink.send(ws::Message::Close(Some(ws::CloseCode::Normal.into())))
        .await
        .unwrap();
    let item = rx.recv().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(None));
    assert!(rx.recv().await.is_none());

    // the peer's close frame is a reply, it is not answered again
    assert_eq!(
        frames
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap(),
        vec![ws::Frame::Close(Some(ws::CloseCode::Normal.into()))]
    );
}

/// STATEMENT: WebSocket handshake failures retain the configured service identity.
///
/// HOW:
/// - Return a regular OK response to an upgrade request using configuration tagged `ws-client-svc`.
/// - Assert `WsClientError::InvalidResponseStatus` and the diagnostic's matching service name.
#[ntex::test]
async fn test_handshake_error_has_service() {
    use ntex::error::ErrorDiagnostic;

    // the upgrade request is answered with a regular response
    let srv = test_server(async |_| HttpService::new(async |_| Ok::<_, io::Error>(Response::Ok())));

    let cfg = SharedCfg::new("WS-TEST")
        .service("ws-client-svc")
        .add(ws::WsClientConfig::new().set_address(srv.addr()));
    let err = ws::WsClient::new(format!("http://{}/", srv.addr()), cfg)
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(
        *err,
        ws::error::WsClientError::InvalidResponseStatus(_)
    ));
    assert_eq!(err.service(), Some("ws-client-svc"));
}

/// STATEMENT: The WebSocket `Host` header contains only the authority without user information.
///
/// HOW:
/// - Capture the upgrade request's `HOST` header while connecting with a URL containing user information and an explicit socket address.
/// - Require successful negotiation and assert the captured header equals the socket address string.
#[ntex::test]
async fn test_host_header_excludes_userinfo() {
    let (host_tx, host_rx) = std::sync::mpsc::channel();
    let srv = test_server(async move |_| {
        let host_tx = host_tx.clone();
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();
                    let _ = host_tx.send(req.head().headers.get(ntex::http::header::HOST).cloned());
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();
                    let _ = io.recv(&ws::Codec::default()).await;
                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    let addr = srv.addr();
    let _con = ws::WsClient::new(
        format!("ws://user:secret@{addr}/"),
        ws::WsClientConfig::new().set_address(addr),
    )
    .connect()
    .await
    .unwrap();

    let host = host_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert_eq!(host, addr.to_string().as_str());
}

/// STATEMENT: A client WebSocket protocol error sends a Protocol close and exhausts the receiver.
///
/// HOW:
/// - After upgrading, have the server send a raw frame with a reserved opcode.
/// - Assert a protocol error followed by EOF and exactly one Protocol close recorded by the server.
#[ntex::test]
async fn test_protocol_error_sends_close() {
    let (frames_tx, frames_rx) = std::sync::mpsc::channel();
    let srv = test_server(async move |_| {
        let frames_tx = frames_tx.clone();
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    // frame with a reserved opcode
                    io.send(Bytes::from_static(&[0x83, 0x00]), &BytesCodec)
                        .await
                        .unwrap();
                    let codec = ws::Codec::default();
                    let mut frames = Vec::new();
                    while let Ok(Some(frame)) = io.recv(&codec).await {
                        frames.push(frame);
                    }
                    let _ = frames_tx.send(frames);
                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    let rx = close_handshake_client(&srv).await.receiver();
    let item = rx.recv().await;
    assert!(
        matches!(item, Some(Err(ws::error::WsError::Protocol(_)))),
        "{item:?}"
    );
    assert!(rx.recv().await.is_none());

    assert_eq!(
        frames_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap(),
        vec![ws::Frame::Close(Some(ws::CloseCode::Protocol.into()))]
    );
}
