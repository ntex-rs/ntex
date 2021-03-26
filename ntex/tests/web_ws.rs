use std::io;

use futures::{SinkExt, StreamExt};
use ntex::http::StatusCode;
use ntex::service::{fn_factory_with_config, fn_service};
use ntex::util::{ByteString, Bytes};
use ntex::web::{self, test, ws, App, HttpRequest};

async fn service(msg: ws::Frame) -> Result<Option<ws::Message>, io::Error> {
    let msg = match msg {
        ws::Frame::Ping(msg) => ws::Message::Pong(msg),
        ws::Frame::Text(text) => {
            ws::Message::Text(String::from_utf8_lossy(&text).as_ref().into())
        }
        ws::Frame::Binary(bin) => ws::Message::Binary(bin),
        ws::Frame::Close(reason) => ws::Message::Close(reason),
        _ => panic!(),
    };
    Ok(Some(msg))
}

#[ntex::test]
async fn web_ws() {
    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(
            |req: HttpRequest, pl: web::types::Payload| async move {
                ws::start::<_, _, _, web::Error>(
                    req,
                    pl,
                    fn_factory_with_config(|_| async {
                        Ok::<_, web::Error>(fn_service(service))
                    }),
                )
                .await
            },
        )))
    });

    // client service
    let mut framed = srv.ws().await.unwrap().into_inner().1;
    framed
        .send(ws::Message::Text(ByteString::from_static("text")))
        .await
        .unwrap();
    let item = framed.next().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Text(Bytes::from_static(b"text")));

    framed
        .send(ws::Message::Binary("text".into()))
        .await
        .unwrap();
    let item = framed.next().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    framed.send(ws::Message::Ping("text".into())).await.unwrap();
    let item = framed.next().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Pong("text".to_string().into()));

    framed
        .send(ws::Message::Close(Some(ws::CloseCode::Normal.into())))
        .await
        .unwrap();

    let item = framed.next().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Normal.into())));
}

#[ntex::test]
async fn web_ws_client() {
    env_logger::init();

    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(
            |req: HttpRequest, pl: web::types::Payload| async move {
                ws::start::<_, _, _, web::Error>(
                    req,
                    pl,
                    fn_factory_with_config(|_| async {
                        Ok::<_, web::Error>(fn_service(service))
                    }),
                )
                .await
            },
        )))
    });

    // client service
    let conn = srv.ws().await.unwrap();
    assert_eq!(conn.response().status(), StatusCode::SWITCHING_PROTOCOLS);

    let sink = conn.sink();
    let mut rx = conn.start_default();

    sink.send(ws::Message::Text(ByteString::from_static("text")))
        .await
        .unwrap();
    let item = rx.next().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Text(Bytes::from_static(b"text")));

    sink.send(ws::Message::Binary("text".into())).await.unwrap();
    let item = rx.next().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    sink.send(ws::Message::Ping("text".into())).await.unwrap();
    let item = rx.next().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Pong("text".to_string().into()));

    let on_disconnect = sink.on_disconnect();

    sink.send(ws::Message::Close(Some(ws::CloseCode::Normal.into())))
        .await
        .unwrap();
    let item = rx.next().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Normal.into())));

    let item = rx.next().await.unwrap();
    assert!(item.is_err());

    on_disconnect.await
}
