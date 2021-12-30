use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::{cell::Cell, future::Future, io, pin::Pin};

use ntex::http::{body, h1, test, HttpService, Request, Response, StatusCode};
use ntex::io::{DispatchItem, Dispatcher, Io};
use ntex::service::{fn_factory, Service};
use ntex::ws::handshake;
use ntex::{util::ByteString, util::Bytes, util::Ready, ws};

struct WsService(Arc<Mutex<Cell<bool>>>);

impl WsService {
    fn new() -> Self {
        WsService(Arc::new(Mutex::new(Cell::new(false))))
    }

    fn set_polled(&self) {
        *self.0.lock().unwrap().get_mut() = true;
    }

    fn was_polled(&self) -> bool {
        self.0.lock().unwrap().get()
    }
}

impl Clone for WsService {
    fn clone(&self) -> Self {
        WsService(self.0.clone())
    }
}

impl Service<(Request, Io, h1::Codec)> for WsService {
    type Response = ();
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), io::Error>>>>;

    fn poll_ready(&self, _ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.set_polled();
        Poll::Ready(Ok(()))
    }

    fn call(&self, (req, io, codec): (Request, Io, h1::Codec)) -> Self::Future {
        let fut = async move {
            let res = handshake(req.head()).unwrap().message_body(());

            io.encode((res, body::BodySize::None).into(), &codec)
                .unwrap();

            Dispatcher::new(io.seal(), ws::Codec::new(), service)
                .await
                .map_err(|_| panic!())
        };

        Box::pin(fut)
    }
}

async fn service(msg: DispatchItem<ws::Codec>) -> Result<Option<ws::Message>, io::Error> {
    let msg = match msg {
        DispatchItem::Item(msg) => match msg {
            ws::Frame::Ping(msg) => ws::Message::Pong(msg),
            ws::Frame::Text(text) => {
                ws::Message::Text(String::from_utf8_lossy(&text).as_ref().into())
            }
            ws::Frame::Binary(bin) => ws::Message::Binary(bin),
            ws::Frame::Continuation(item) => ws::Message::Continuation(item),
            ws::Frame::Close(reason) => ws::Message::Close(reason),
            _ => panic!(),
        },
        _ => return Ok(None),
    };
    Ok(Some(msg))
}

#[ntex::test]
async fn test_simple() {
    let ws_service = WsService::new();
    let mut srv = test::server({
        let ws_service = ws_service.clone();
        move || {
            let ws_service = ws_service.clone();
            HttpService::build()
                .upgrade(fn_factory(move || {
                    Ready::Ok::<_, io::Error>(ws_service.clone())
                }))
                .h1(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
        }
    });

    // client service
    let conn = srv.ws().await.unwrap();
    assert_eq!(conn.response().status(), StatusCode::SWITCHING_PROTOCOLS);

    let (io, codec, _) = conn.into_inner();
    io.send(ws::Message::Text(ByteString::from_static("text")), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Text(Bytes::from_static(b"text"))
    );

    io.send(ws::Message::Binary("text".into()), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Binary(Bytes::from_static(&b"text"[..]))
    );

    io.send(ws::Message::Ping("text".into()), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Pong("text".to_string().into())
    );

    io.send(
        ws::Message::Continuation(ws::Item::FirstText("text".into())),
        &codec,
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::FirstText(Bytes::from_static(b"text")))
    );

    assert!(io
        .send(
            ws::Message::Continuation(ws::Item::FirstText("text".into())),
            &codec,
        )
        .await
        .is_err());
    assert!(io
        .send(
            ws::Message::Continuation(ws::Item::FirstBinary("text".into())),
            &codec,
        )
        .await
        .is_err());

    io.send(
        ws::Message::Continuation(ws::Item::Continue("text".into())),
        &codec,
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Continue(Bytes::from_static(b"text")))
    );

    io.send(
        ws::Message::Continuation(ws::Item::Last("text".into())),
        &codec,
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Last(Bytes::from_static(b"text")))
    );

    assert!(io
        .send(
            ws::Message::Continuation(ws::Item::Continue("text".into())),
            &codec,
        )
        .await
        .is_err());

    assert!(io
        .send(
            ws::Message::Continuation(ws::Item::Last("text".into())),
            &codec,
        )
        .await
        .is_err());

    io.send(
        ws::Message::Continuation(ws::Item::FirstBinary(Bytes::from_static(b"bin"))),
        &codec,
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::FirstBinary(Bytes::from_static(b"bin")))
    );

    io.send(
        ws::Message::Continuation(ws::Item::Continue("text".into())),
        &codec,
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Continue(Bytes::from_static(b"text")))
    );

    io.send(
        ws::Message::Continuation(ws::Item::Last("text".into())),
        &codec,
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Last(Bytes::from_static(b"text")))
    );

    io.send(
        ws::Message::Close(Some(ws::CloseCode::Normal.into())),
        &codec,
    )
    .await
    .unwrap();

    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Close(Some(ws::CloseCode::Normal.into()))
    );

    assert!(ws_service.was_polled());
}
