use std::io;

use ntex::http::{StatusCode, header};
use ntex::service::{fn_factory_with_config, fn_service};
use ntex::util::{ByteString, Bytes};
use ntex::web::{self, App, HttpRequest, HttpResponse, test, ws};
use ntex::ws::error::WsClientError;

async fn service(msg: ws::Frame) -> Result<Option<ws::Message>, io::Error> {
    let msg = match msg {
        ws::Frame::Ping(msg) => ws::Message::Pong(msg),
        ws::Frame::Text(text) => {
            ws::Message::Text(String::from_utf8_lossy(&text).as_ref().into())
        }
        ws::Frame::Binary(bin) => ws::Message::Binary(bin),
        ws::Frame::Close(_) => ws::Message::Close(Some(ws::CloseCode::Away.into())),
        _ => panic!(),
    };
    Ok(Some(msg))
}

#[ntex::test]
async fn web_ws() {
    let _ = env_logger::try_init();

    let srv = test::server(async || {
        App::new().service(web::resource("/").route(web::to(
            |req: HttpRequest| async move {
                ws::start::<_, _, web::Error>(
                    req,
                    fn_factory_with_config(|_| async {
                        Ok::<_, web::Error>(fn_service(service))
                    }),
                )
                .await
            },
        )))
    })
    .await;

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
    let srv = test::server(async || {
        App::new()
            .service(web::resource("/").route(web::to(|| async { HttpResponse::Ok() })))
            .service(web::resource("/ws_error").route(web::to(|| async {
                Err::<HttpResponse, _>(io::Error::other("test"))
            })))
    })
    .await;

    assert!(matches!(
        srv.ws().await.err().unwrap(),
        WsClientError::InvalidResponseStatus(StatusCode::OK)
    ));
    assert!(matches!(
        srv.ws_at("/ws_error").await.err().unwrap(),
        WsClientError::InvalidResponseStatus(StatusCode::INTERNAL_SERVER_ERROR)
    ));
}

#[ntex::test]
async fn web_no_ws_2() {
    let srv = test::server(async || {
        App::new().service(
            web::resource("/")
                .route(web::to(|| async { HttpResponse::Ok().body("Hello world") })),
        )
    })
    .await;

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
    let srv = test::server(async || {
        App::new().service(web::resource("/").route(web::to(
            |req: HttpRequest| async move {
                ws::start::<_, _, web::Error>(
                    req,
                    fn_factory_with_config(|_| async {
                        Ok::<_, web::Error>(fn_service(service))
                    }),
                )
                .await
            },
        )))
    })
    .await;

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

    let item = rx.recv().await;
    assert!(item.is_none());

    // TODO fix
    on_disconnect.await
}

#[ntex::test]
async fn web_ws_subprotocol() {
    use ntex::service::cfg::SharedCfg;
    use ntex::time::Seconds;
    use ntex::ws::WsClient;

    let srv = test::server(async || {
        App::new().service(web::resource("/").route(web::to(
            |req: HttpRequest| async move {
                // choose first supported protocol, convert to owned String
                let protocol: Option<String> = ws::subprotocols(&req)
                    .find(|p| *p == "my-subprotocol" || *p == "others-subprotocol")
                    .map(String::from);

                ws::start_using_subprotocol::<_, _, _, web::Error>(
                    req,
                    protocol,
                    fn_factory_with_config(|_| async {
                        Ok::<_, web::Error>(fn_service(service))
                    }),
                )
                .await
            },
        )))
    })
    .await;

    // client requests subprotocol
    let conn = WsClient::build(srv.url("/"))
        .address(srv.addr())
        .timeout(Seconds(30))
        .protocols(["my-subprotocol"])
        .finish(SharedCfg::default())
        .await
        .unwrap()
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
async fn web_ws_subprotocol_none() {
    use ntex::service::cfg::SharedCfg;
    use ntex::time::Seconds;
    use ntex::ws::WsClient;

    let srv = test::server(async || {
        App::new().service(web::resource("/").route(web::to(
            |req: HttpRequest| async move {
                // choose first supported protocol (none will match), convert to owned String
                let protocol: Option<String> = ws::subprotocols(&req)
                    .find(|p| *p == "unsupported")
                    .map(String::from);

                ws::start_using_subprotocol::<_, _, _, web::Error>(
                    req,
                    protocol,
                    fn_factory_with_config(|_| async {
                        Ok::<_, web::Error>(fn_service(service))
                    }),
                )
                .await
            },
        )))
    })
    .await;

    // client requests subprotocol that server doesn't support
    let conn = WsClient::build(srv.url("/"))
        .address(srv.addr())
        .timeout(Seconds(30))
        .protocols(["my-subprotocol"])
        .finish(SharedCfg::default())
        .await
        .unwrap()
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

    let srv = test::server(async || {
        App::new().service(web::resource("/").route(web::to(
            |req: HttpRequest| async move {
                // collect all requested protocols into owned Strings
                let protocols: Vec<String> =
                    ws::subprotocols(&req).map(String::from).collect();

                // choose based on priority
                let protocol = protocols
                    .iter()
                    .find(|p| *p == "proto2")
                    .or_else(|| protocols.iter().find(|p| *p == "proto1"))
                    .cloned();

                ws::start_using_subprotocol::<_, _, _, web::Error>(
                    req,
                    protocol,
                    fn_factory_with_config(|_| async {
                        Ok::<_, web::Error>(fn_service(service))
                    }),
                )
                .await
            },
        )))
    })
    .await;

    // client requests multiple protocols (comma-separated)
    let conn = WsClient::build(srv.url("/"))
        .address(srv.addr())
        .timeout(Seconds(30))
        .protocols(["proto1", "proto2"])
        .finish(SharedCfg::default())
        .await
        .unwrap()
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
