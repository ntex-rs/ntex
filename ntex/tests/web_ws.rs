use std::io;

use ntex::http::{StatusCode, header};
use ntex::web::{self, App, HttpRequest, HttpResponse, test, ws};
use ntex::ws::{
    WsClientConfig,
    error::{HandshakeError, WsClientError, WsError},
};
use ntex::{service, util::ByteString, util::Bytes};

async fn ws_service(msg: ws::Frame) -> Result<Option<ws::Message>, io::Error> {
    let msg = match msg {
        ws::Frame::Ping(msg) => ws::Message::Pong(msg),
        ws::Frame::Text(text) => ws::Message::Text(String::from_utf8_lossy(&text).as_ref().into()),
        ws::Frame::Binary(bin) => ws::Message::Binary(bin),
        ws::Frame::Close(_) => ws::Message::Close(Some(ws::CloseCode::Away.into())),
        _ => panic!(),
    };
    Ok(Some(msg))
}

/// STATEMENT: A web WebSocket handler exchanges data frames and supplies its chosen close reason.
///
/// HOW:
/// - Start `ws_service` through `ws::start` and verify text and binary echoes plus ping/pong.
/// - Send a Normal close and assert the handler replies with Away.
#[ntex::test]
async fn web_ws() {
    let _ = env_logger::try_init();

    let srv = test::server(async |_| {
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                let _ = ws::start(&req, None, ws_service).await;
            })),
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
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Away.into())));
}

/// STATEMENT: WebSocket clients report ordinary and error HTTP responses as invalid upgrade statuses.
///
/// HOW:
/// - Connect to a normal OK web route and a route returning an I/O error.
/// - Assert invalid-response-status variants for 200 and 500 and their exact display messages.
#[ntex::test]
async fn web_no_ws() {
    let srv = test::server(async |_| {
        App::new()
            .service(web::resource("/").route(web::to(async || HttpResponse::Ok())))
            .service(web::resource("/ws_error").route(web::to(async || {
                Err::<HttpResponse, _>(io::Error::other("test"))
            })))
    });

    let err = srv.ws().await.err().unwrap();
    assert!(matches!(
        *err,
        WsClientError::InvalidResponseStatus(StatusCode::OK)
    ));
    assert_eq!(err.to_string(), "Invalid response status: 200 OK");

    let err = srv.ws_at("/ws_error").await.err().unwrap();
    assert!(matches!(
        *err,
        WsClientError::InvalidResponseStatus(StatusCode::INTERNAL_SERVER_ERROR)
    ));
    assert_eq!(
        err.to_string(),
        "Invalid response status: 500 Internal Server Error"
    );
}

/// STATEMENT: A WebSocket handshake does not inherit a recycled POST method from the request-head pool.
///
/// HOW:
/// - Complete a POST so its `RequestHead` returns to the thread-local pool, then create a WebSocket client on the same thread.
/// - Assert the POST is OK, negotiation succeeds, and a text message echoes correctly.
#[ntex::test]
async fn web_ws_after_pooled_post_request() {
    let srv = test::server(async |_| {
        App::new()
            .service(
                web::resource("/").route(web::to(async move |req: HttpRequest| {
                    let _ = ws::start(&req, None, ws_service).await;
                })),
            )
            .service(web::resource("/post").route(web::post().to(async || HttpResponse::Ok())))
    });

    // a completed POST request releases its RequestHead back to the
    // thread-local message pool; a ws client built afterwards on the same
    // thread must not reuse the recycled POST method for its handshake
    let res = srv.post("/post").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let (io, codec, _) = srv.ws().await.unwrap().into_inner();
    io.send(ws::Message::Text(ByteString::from_static("text")), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Text(Bytes::from_static(b"text")));
}

/// STATEMENT: An unhandled upgrade request still delivers a normal web response body.
///
/// HOW:
/// - Send GET with upgrade connection tokens to a route returning `Hello world`.
/// - Assert a successful response and the complete ordinary body.
#[ntex::test]
async fn web_no_ws_2() {
    let srv = test::server(async |_| {
        App::new().service(
            web::resource("/").route(web::to(async || HttpResponse::Ok().body("Hello world"))),
        )
    });

    let response = srv
        .get("/")
        .no_decompress()
        .header("test", "h2c")
        .header("connection", "upgrade, test")
        .set_connection_type(ntex::http::ConnectionType::Upgrade)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body = response.body().await.unwrap();
    assert_eq!(body, b"Hello world");
}

/// STATEMENT: WebSocket sink and receiver APIs exchange messages and observe closing-handshake completion.
///
/// HOW:
/// - Assert switching protocols, then verify text/binary echoes and ping/pong through the sink and receiver.
/// - Send Normal close, assert an Away reply, await disconnect, and assert receiver EOF.
#[ntex::test]
async fn web_ws_client() {
    let srv = test::server(async |_| {
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                let _ = ws::start(&req, None, ws_service).await;
            })),
        )
    });

    // client service
    let conn = srv.ws().await.unwrap();
    assert_eq!(conn.response().status(), StatusCode::SWITCHING_PROTOCOLS);

    let sink = conn.sink();
    let rx = conn.receiver();

    sink.send(ws::Message::Text(ByteString::from_static("text")))
        .await
        .unwrap();
    let item = rx.recv().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Text(Bytes::from_static(b"text")));

    sink.send(ws::Message::Binary("text".into())).await.unwrap();
    let item = rx.recv().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    sink.send(ws::Message::Ping("text".into())).await.unwrap();
    let item = rx.recv().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Pong("text".to_string().into()));

    let on_disconnect = sink.on_disconnect();

    sink.send(ws::Message::Close(Some(ws::CloseCode::Normal.into())))
        .await
        .unwrap();
    let item = rx.recv().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Away.into())));

    on_disconnect.await;
    assert!(rx.recv().await.is_none());
}

/// STATEMENT: A client service stops within its close timeout when the server does not answer a close.
///
/// HOW:
/// - Use a server service that echoes text but ignores close messages and configure a 50 ms client close timeout.
/// - Send text, run a client service that responds with close, and assert its start future completes within 500 ms.
#[ntex::test]
async fn web_ws_service_close_timeout() {
    use ntex::io::DispatchItem;
    use ntex::time::{Millis, timeout};
    use ntex::ws::WsClient;

    let srv = test::server(async |_| {
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                let _ = ws::start_with(
                    &req,
                    None,
                    service::fn_service(async |item: DispatchItem<ws::WsSink>| {
                        let msg = match item {
                            DispatchItem::Item(ws::Frame::Text(text)) => Some(ws::Message::Text(
                                String::from_utf8_lossy(&text).as_ref().into(),
                            )),
                            _ => None,
                        };
                        Ok::<_, WsError<io::Error>>(msg)
                    }),
                )
                .await;
            })),
        )
    });

    let conn = WsClient::new(
        srv.url("/"),
        WsClientConfig::new()
            .set_address(srv.addr())
            .set_close_timeout(Millis(50)),
    )
    .connect()
    .await
    .unwrap();
    conn.sink()
        .send(ws::Message::Text(ByteString::from_static("text")))
        .await
        .unwrap();

    let result = timeout(
        Millis(500),
        conn.seal()
            .start(service::fn_service(async |frame: ws::Frame| {
                Ok::<_, ()>(match frame {
                    ws::Frame::Text(_) => Some(ws::Message::Close(None)),
                    _ => None,
                })
            })),
    )
    .await;
    assert!(result.is_ok());
}

/// STATEMENT: A requested supported WebSocket subprotocol is returned by the handshake.
///
/// HOW:
/// - Have the web handler select the first supported offered protocol and request `my-subprotocol`.
/// - Assert switching protocols and `SEC_WEBSOCKET_PROTOCOL` equal to the selected protocol.
#[ntex::test]
async fn web_ws_subprotocol() {
    use ntex::{time::Seconds, ws::WsClient};

    let srv = test::server(async |_| {
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                // choose first supported protocol, convert to owned String
                let protocol: Option<&str> = ws::subprotocols(&req)
                    .find(|p| *p == "my-subprotocol" || *p == "others-subprotocol");

                let _ = ws::start(&req, protocol, ws_service).await;
            })),
        )
    });

    // client requests subprotocol
    let conn = WsClient::new(
        srv.url("/"),
        WsClientConfig::new()
            .set_address(srv.addr())
            .set_handshake_timeout(Seconds(30))
            .set_protocols(["my-subprotocol"])
            .unwrap(),
    )
    .connect()
    .await
    .unwrap();

    assert_eq!(conn.response().status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        conn.response()
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .map(|v| v.to_str().unwrap()),
        Some("my-subprotocol")
    );
}

/// STATEMENT: A web handler cannot select a WebSocket subprotocol the client did not request.
///
/// HOW:
/// - Offer `chat` while the handler calls `ws::start` with `other`.
/// - Assert the client sees HTTP 400 and the handler reports `HandshakeError::BadWebsocketProtocol`.
#[ntex::test]
async fn web_ws_rejects_unrequested_subprotocol() {
    use std::sync::mpsc;

    use ntex::ws::WsClient;

    let (tx, rx) = mpsc::channel();
    let srv = test::server(async move |_| {
        let tx = tx.clone();
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                let result = ws::start(&req, Some("other"), ws_service).await;
                tx.send(matches!(
                    result,
                    Err(WsError::Handshake(HandshakeError::BadWebsocketProtocol))
                ))
                .unwrap();
            })),
        )
    });

    let result = WsClient::new(
        srv.url("/"),
        WsClientConfig::new()
            .set_address(srv.addr())
            .set_protocols(["chat"])
            .unwrap(),
    )
    .connect()
    .await;

    let err = result.map(|_| ()).unwrap_err();
    assert!(matches!(
        *err,
        WsClientError::InvalidResponseStatus(StatusCode::BAD_REQUEST)
    ));
    assert!(rx.recv().unwrap());
}

/// STATEMENT: An unsupported WebSocket version receives a bad-request response advertising version 13.
///
/// HOW:
/// - Send an upgrade request with WebSocket version `8` to a route using `ws::start`.
/// - Assert HTTP 400 and a `sec-websocket-version` response header of `13`.
#[ntex::test]
async fn web_ws_handshake_error_response() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async |req: HttpRequest| {
            let _ = ws::start(&req, None, ws_service).await;
        })))
    });

    let response = srv
        .get("/")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "8")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .set_connection_type(ntex::http::ConnectionType::Upgrade)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get("sec-websocket-version").unwrap(),
        "13"
    );
}

/// STATEMENT: A WebSocket URL's authority, including its nondefault port, becomes the `Host` header.
///
/// HOW:
/// - Connect using an `example.test` URL while directing the socket to the local test server.
/// - Assert switching protocols and the handler's captured `HOST` equals the full URL authority.
#[ntex::test]
async fn web_ws_host_includes_port() {
    use std::sync::mpsc;

    use ntex::ws::WsClient;

    let (tx, rx) = mpsc::channel();
    let srv = test::server(async move |_| {
        let tx = tx.clone();
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                tx.send(
                    req.headers()
                        .get(header::HOST)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                )
                .unwrap();
                let _ = ws::start(&req, None, ws_service).await;
            })),
        )
    });

    let authority = format!("example.test:{}", srv.addr().port());
    let conn = WsClient::new(
        format!("ws://{authority}/"),
        WsClientConfig::new().set_address(srv.addr()),
    )
    .connect()
    .await
    .unwrap();

    assert_eq!(conn.response().status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(rx.recv().unwrap(), authority);
}

/// STATEMENT: A WebSocket handshake succeeds without a subprotocol when no offered protocol is supported.
///
/// HOW:
/// - Offer `my-subprotocol` while the handler searches only for an unsupported value.
/// - Assert switching protocols and no `SEC_WEBSOCKET_PROTOCOL` response header.
#[ntex::test]
async fn web_ws_subprotocol_none() {
    use ntex::{time::Seconds, ws::WsClient};

    let srv = test::server(async |_| {
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                // choose first supported protocol (none will match), convert to owned String
                let protocol: Option<&str> = ws::subprotocols(&req).find(|p| *p == "unsupported");

                let _ = ws::start(&req, protocol, ws_service).await;
            })),
        )
    });

    // client requests subprotocol that server doesn't support
    let conn = WsClient::new(
        srv.url("/"),
        WsClientConfig::new()
            .set_address(srv.addr())
            .set_handshake_timeout(Seconds(30))
            .set_protocols(["my-subprotocol"])
            .unwrap(),
    )
    .connect()
    .await
    .unwrap();

    assert_eq!(conn.response().status(), StatusCode::SWITCHING_PROTOCOLS);
    // no protocol header in response
    assert!(
        conn.response()
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .is_none()
    );
}

/// STATEMENT: WebSocket subprotocol parsing lets the handler select by its own priority.
///
/// HOW:
/// - Offer `proto1` and `proto2`; collect `ws::subprotocols` and prefer `proto2` in the handler.
/// - Assert switching protocols and a `proto2` response protocol header.
#[ntex::test]
async fn web_ws_protocols_parsing() {
    use ntex::service::cfg::SharedCfg;
    use ntex::time::Seconds;
    use ntex::ws::WsClient;

    let srv = test::server(async |_| {
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                // collect all requested protocols into owned Strings
                let protocols: Vec<String> = ws::subprotocols(&req).map(String::from).collect();

                // choose based on priority
                let protocol = protocols
                    .iter()
                    .find(|p| *p == "proto2")
                    .or_else(|| protocols.iter().find(|p| *p == "proto1"))
                    .map(|s| s.as_ref());

                let _ = ws::start(&req, protocol, ws_service).await;
            })),
        )
    });

    // client requests multiple protocols (comma-separated)
    let conn = WsClient::new(
        srv.url("/"),
        SharedCfg::new("C").add(
            WsClientConfig::new()
                .set_address(srv.addr())
                .set_handshake_timeout(Seconds(30))
                .set_protocols(["proto1", "proto2"])
                .unwrap(),
        ),
    )
    .connect()
    .await
    .unwrap();

    assert_eq!(conn.response().status(), StatusCode::SWITCHING_PROTOCOLS);
    // server chooses proto2 (higher priority)
    assert_eq!(
        conn.response()
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .map(|v| v.to_str().unwrap()),
        Some("proto2")
    );
}

/// STATEMENT: Closing a web WebSocket connection invokes the underlying service shutdown hook.
///
/// HOW:
/// - Attach a shutdown notification to `ws_service` and verify a text echo.
/// - Send Normal close, assert the Away reply, and require the shutdown notification within one second.
#[ntex::test]
async fn web_ws_shutdown_propagation() {
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    let srv = test::server(async move |_| {
        let shutdown_tx = shutdown_tx.clone();
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                let shutdown_tx = shutdown_tx.clone();
                let _ = ws::start(
                    &req,
                    None,
                    service(ws_service).shutdown(async move |_| {
                        let _ = shutdown_tx.send(());
                    }),
                )
                .await;
            })),
        )
    });

    // make ure the server is working
    let (io, codec, _) = srv.ws().await.unwrap().into_inner();
    io.send(ws::Message::Text(ByteString::from_static("test")), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Text(Bytes::from_static(b"test")));

    // close the connection to trigger shutdown
    io.send(
        ws::Message::Close(Some(ws::CloseCode::Normal.into())),
        &codec,
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Away.into())));

    shutdown_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("Service shutdown was not called");
}

/// STATEMENT: The service-provided WebSocket sink rejects sends after the service has sent a close.
///
/// HOW:
/// - Have the service return `Close(None)` for `close` and verify the peer receives it.
/// - Send another text frame to trigger a sink send and assert that send fails.
#[ntex::test]
async fn web_ws_sink_shares_close_state() {
    let (tx, rx) = std::sync::mpsc::channel();
    let srv = test::server(async move |_| {
        let tx = tx.clone();
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                let tx = tx.clone();
                let _ = ws::start(
                    &req,
                    None,
                    service::fn_service_st(async move |sink: &ws::WsSink, frame: ws::Frame| {
                        match frame {
                            ws::Frame::Text(txt) if txt == "close" => {
                                Ok(Some(ws::Message::Close(None)))
                            }
                            ws::Frame::Text(_) => {
                                // the service already sent a close message
                                let res = sink.send(ws::Message::Text("late".into())).await;
                                let _ = tx.send(res.is_err());
                                Ok::<_, io::Error>(None)
                            }
                            _ => Ok(None),
                        }
                    }),
                )
                .await;
            })),
        )
    });

    let (io, codec, _) = srv.ws().await.unwrap().into_inner();
    io.send(ws::Message::Text("close".into()), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(None));

    io.send(ws::Message::Text("send".into()), &codec)
        .await
        .unwrap();
    assert!(
        rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap(),
        "sink sent a message after the close message"
    );
}

/// STATEMENT: A web WebSocket protocol error emits a Protocol close and propagates an error from `ws::start`.
///
/// HOW:
/// - Send a raw masked frame with a reserved opcode after negotiation.
/// - Assert the peer receives a Protocol close and the handler reports `WsError::Protocol`.
#[ntex::test]
async fn web_ws_protocol_error_sends_close() {
    let (tx, rx) = std::sync::mpsc::channel();
    let srv = test::server(async move |_| {
        let tx = tx.clone();
        App::new().service(
            web::resource("/").route(web::to(async move |req: HttpRequest| {
                let res = ws::start(&req, None, ws_service).await;
                let _ = tx.send(matches!(res, Err(WsError::Protocol(_))));
            })),
        )
    });

    let (io, codec, _) = srv.ws().await.unwrap().into_inner();
    // frame with a reserved opcode
    io.send(
        Bytes::from_static(&[0x83, 0x80, 0, 0, 0, 0]),
        &ntex::codec::BytesCodec,
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Protocol.into())));
    assert!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap());
}
