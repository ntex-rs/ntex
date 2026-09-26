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

    assert!(result.is_err());
    assert!(rx.recv().unwrap());
}

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
