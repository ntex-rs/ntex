use std::{cell::Cell, io, sync::Arc, sync::Mutex};

use ntex::codec::BytesCodec;
use ntex::http::test::server as test_server;
use ntex::http::{HttpService, HttpServiceConfig, Request, Response, StatusCode, body, h1, test};
use ntex::io::{DispatchItem, Dispatcher, Io, IoConfig};
use ntex::service::{Ctx, Pipeline, Service, cfg::SharedCfg};
use ntex::time::Seconds;
use ntex::util::{ByteString, Bytes};
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

impl Service<(), (Request, Io, h1::Codec)> for WsService {
    type Res = ();
    type Error = io::Error;

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        self.set_polled();
        Ok(())
    }

    async fn call(
        &self,
        (req, io, codec): (Request, Io, h1::Codec),
        _: Ctx<'_, Self>,
    ) -> Result<(), io::Error> {
        let res = handshake(req.head()).unwrap().message_body(());

        io.encode((res, body::BodySize::None).into(), &codec)
            .unwrap();

        // SAFETY: no reference returned by `io.cfg()` is retained across the
        // protocol transition.
        unsafe {
            io.set_config(
                SharedCfg::new("WS-SRV").add(IoConfig::new().set_keepalive_timeout(Seconds(0))),
            );
        }
        Dispatcher::new(io.seal(), ws::Codec::new(), Pipeline::new((), service))
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
    let srv = test::server_with_config(
        {
            let ws_service = ws_service.clone();
            async move |_| {
                let ws_service = ws_service.clone();
                HttpService::h1(async |_| Ok::<_, io::Error>(Response::NotFound())).control(
                    async move |req: h1::Control<_, _>| {
                        let ack = if let h1::Control::Upgrade(upg) = req {
                            assert!(format!("{upg:?}").contains("Upgrade"));
                            let ws_service = Pipeline::new((), ws_service.clone());
                            let (ack, io, req, codec) = upg.handle();
                            let _ = ws_service.call((req, io, codec)).await;
                            ack
                        } else {
                            req.ack()
                        };
                        Ok::<_, io::Error>(ack)
                    },
                )
            }
        },
        SharedCfg::new("SRV").add(
            HttpServiceConfig::new()
                .set_keepalive(1)
                .set_headers_read_rate(Seconds(1), Seconds::ZERO, 16)
                .set_payload_read_rate(Seconds(1), Seconds::ZERO, 16),
        ),
    );

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

    assert!(
        io.send(
            ws::Message::Continuation(ws::Item::FirstText("text".into())),
            &codec,
        )
        .await
        .is_err()
    );
    assert!(
        io.send(
            ws::Message::Continuation(ws::Item::FirstBinary("text".into())),
            &codec,
        )
        .await
        .is_err()
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

    assert!(
        io.send(
            ws::Message::Continuation(ws::Item::Continue("text".into())),
            &codec,
        )
        .await
        .is_err()
    );

    assert!(
        io.send(
            ws::Message::Continuation(ws::Item::Last("text".into())),
            &codec,
        )
        .await
        .is_err()
    );

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
    let srv = test_server(async |_| {
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();

                    // send handshake respone
                    let res = handshake_response(req.head()).build();
                    io.encode(
                        h1::Message::Item((res.drop_body(), body::BodySize::None)),
                        &codec,
                    )
                    .unwrap();

                    // start websocket service
                    let io = ws::WsTransport::create(io, ws::Codec::default());
                    while let Some(item) = io.recv(&BytesCodec).await.map_err(|e| e.into_inner())? {
                        io.send(item, &BytesCodec).await.unwrap()
                    }

                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    // client service
    let io = srv.ws().await.unwrap().into_inner().0;

    let codec = ws::Codec::default().set_client_mode();
    io.send(ws::Message::Binary(Bytes::from_static(b"text")), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    // the transport echoes the close code
    io.send(ws::Message::Close(Some(ws::CloseCode::Away.into())), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Away.into())));
}
