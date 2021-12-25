use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::{cell::Cell, future::Future, io, pin::Pin};

use ntex::codec::BytesCodec;
use ntex::http::test::server as test_server;
use ntex::http::ws::handshake_response;
use ntex::http::{
    body, h1, test, ws::handshake, HttpService, Request, Response, StatusCode,
};
use ntex::io::{DispatchItem, Dispatcher, Io, Timer};
use ntex::service::{fn_factory, Service};
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

            Dispatcher::new(io.seal(), ws::Codec::new(), service, Timer::default())
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
    io.send(&codec, ws::Message::Text(ByteString::from_static("text")))
        .await
        .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Text(Bytes::from_static(b"text"))
    );

    io.send(&codec, ws::Message::Binary("text".into()))
        .await
        .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Binary(Bytes::from_static(&b"text"[..]))
    );

    io.send(&codec, ws::Message::Ping("text".into()))
        .await
        .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Pong("text".to_string().into())
    );

    io.send(
        &codec,
        ws::Message::Continuation(ws::Item::FirstText("text".into())),
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
            &codec,
            ws::Message::Continuation(ws::Item::FirstText("text".into())),
        )
        .await
        .is_err());
    assert!(io
        .send(
            &codec,
            ws::Message::Continuation(ws::Item::FirstBinary("text".into())),
        )
        .await
        .is_err());

    io.send(
        &codec,
        ws::Message::Continuation(ws::Item::Continue("text".into())),
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Continue(Bytes::from_static(b"text")))
    );

    io.send(
        &codec,
        ws::Message::Continuation(ws::Item::Last("text".into())),
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
            &codec,
            ws::Message::Continuation(ws::Item::Continue("text".into())),
        )
        .await
        .is_err());

    assert!(io
        .send(
            &codec,
            ws::Message::Continuation(ws::Item::Last("text".into())),
        )
        .await
        .is_err());

    io.send(
        &codec,
        ws::Message::Continuation(ws::Item::FirstBinary(Bytes::from_static(b"bin"))),
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::FirstBinary(Bytes::from_static(b"bin")))
    );

    io.send(
        &codec,
        ws::Message::Continuation(ws::Item::Continue("text".into())),
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Continue(Bytes::from_static(b"text")))
    );

    io.send(
        &codec,
        ws::Message::Continuation(ws::Item::Last("text".into())),
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Last(Bytes::from_static(b"text")))
    );

    io.send(
        &codec,
        ws::Message::Close(Some(ws::CloseCode::Normal.into())),
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

#[ntex::test]
async fn test_transport() {
    let mut srv = test_server(|| {
        HttpService::build()
            .upgrade(|(req, io, codec): (Request, Io, h1::Codec)| {
                async move {
                    let res = handshake_response(req.head()).finish();

                    // send handshake respone
                    io.encode(
                        h1::Message::Item((res.drop_body(), body::BodySize::None)),
                        &codec,
                    )
                    .unwrap();

                    let io = io
                        .add_filter(ws::WsTransportFactory::new(ws::Codec::default()))
                        .await?;

                    // start websocket service
                    loop {
                        if let Some(item) =
                            io.recv(&BytesCodec).await.map_err(|e| e.into_inner())?
                        {
                            io.send(&BytesCodec, item.freeze()).await.unwrap()
                        } else {
                            break;
                        }
                    }
                    Ok::<_, io::Error>(())
                }
            })
            .finish(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
    });

    // client service
    let io = srv.ws().await.unwrap().into_inner().0;

    let codec = ws::Codec::default().client_mode();
    io.send(&codec, ws::Message::Binary(Bytes::from_static(b"text")))
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));
}
