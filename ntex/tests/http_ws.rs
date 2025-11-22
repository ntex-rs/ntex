use std::{cell::Cell, io, sync::Arc, sync::Mutex};

use ntex::codec::BytesCodec;
use ntex::http::test::server as test_server;
use ntex::http::{body, h1, test, HttpService, Request, Response, StatusCode};
use ntex::io::{DispatchItem, Dispatcher, Io, IoConfig};
use ntex::service::{Pipeline, Service, ServiceCtx};
use ntex::time::Seconds;
use ntex::util::{ByteString, Bytes, Ready};
use ntex::ws::{self, handshake, handshake_response};

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

    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        self.set_polled();
        Ok(())
    }

    async fn call(
        &self,
        (req, io, codec): (Request, Io, h1::Codec),
        _: ServiceCtx<'_, Self>,
    ) -> Result<(), io::Error> {
        let res = handshake(req.head()).unwrap().message_body(());

        io.encode((res, body::BodySize::None).into(), &codec)
            .unwrap();

        io.set_config(
            IoConfig::build("WS-SRV")
                .set_keepalive_timeout(Seconds(0))
                .finish(),
        );
        Dispatcher::new(io.seal(), ws::Codec::new(), service)
            .await
            .map_err(|_| panic!())
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
            let ws_service = Pipeline::new(ws_service.clone());
            HttpService::build()
                .keep_alive(1)
                .headers_read_rate(Seconds(1), Seconds::ZERO, 16)
                .payload_read_rate(Seconds(1), Seconds::ZERO, 16)
                .h1_control(move |req: h1::Control<_, _>| {
                    let ack = if let h1::Control::Upgrade(upg) = req {
                        let ws_service = ws_service.clone();
                        upg.handle(|req, io, codec| async move {
                            ws_service.call((req, io, codec)).await
                        })
                    } else {
                        req.ack()
                    };
                    async move { Ok::<_, io::Error>(ack) }
                })
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

#[ntex::test]
async fn test_transport() {
    let mut srv = test_server(|| {
        HttpService::build()
            .h1_control(move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    upg.handle(|req, io, codec| async move {
                        let res = handshake_response(req.head()).finish();

                        // send handshake respone
                        io.encode(
                            h1::Message::Item((res.drop_body(), body::BodySize::None)),
                            &codec,
                        )
                        .unwrap();

                        let io = ws::WsTransport::create(io, ws::Codec::default());

                        // start websocket service
                        while let Some(item) =
                            io.recv(&BytesCodec).await.map_err(|e| e.into_inner())?
                        {
                            io.send(item.freeze(), &BytesCodec).await.unwrap()
                        }
                        Ok::<_, io::Error>(())
                    })
                } else {
                    req.ack()
                };
                async move { Ok::<_, io::Error>(ack) }
            })
            .finish(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
    });

    // client service
    let io = srv.ws().await.unwrap().into_inner().0;

    let codec = ws::Codec::default().client_mode();
    io.send(ws::Message::Binary(Bytes::from_static(b"text")), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    io.send(ws::Message::Close(None), &codec).await.unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(
        item,
        ws::Frame::Close(Some(ws::CloseReason {
            code: ws::CloseCode::Normal,
            description: None
        }))
    );
}
