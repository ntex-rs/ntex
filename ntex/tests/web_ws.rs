use std::io;

use ntex::http::StatusCode;
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
