use std::{cell::Cell, io, sync::Arc, sync::Mutex};

use ntex::codec::BytesCodec;
use ntex::http::test::server as test_server;
use ntex::http::{
    HttpService, HttpServiceConfig, Request, Response, StatusCode, body, h1, test,
};
use ntex::io::{DispatchItem, Dispatcher, Io, IoConfig};
use ntex::service::{Pipeline, Service, ServiceCtx, cfg::SharedCfg};
use ntex::time::{Millis, Seconds, sleep};
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
            SharedCfg::new("WS-SRV").add(IoConfig::new().set_keepalive_timeout(Seconds(0))),
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
    let srv = test::server_with_config(
        {
            let ws_service = ws_service.clone();
            async move || {
                let ws_service = Pipeline::new(ws_service.clone());
                HttpService::h1(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
                    .control(move |req: h1::Control<_, _>| {
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
            }
        },
        SharedCfg::new("SRV").add(
            HttpServiceConfig::new()
                .set_keepalive(1)
                .set_headers_read_rate(Seconds(1), Seconds::ZERO, 16)
                .set_payload_read_rate(Seconds(1), Seconds::ZERO, 16),
        ),
    )
    .await;

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
    let srv = test_server(async || {
        HttpService::new(|_| Ready::Ok::<_, io::Error>(Response::NotFound())).h1_control(
            move |req: h1::Control<_, _>| {
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
                            io.send(item, &BytesCodec).await.unwrap()
                        }
                        Ok::<_, io::Error>(())
                    })
                } else {
                    req.ack()
                };
                async move { Ok::<_, io::Error>(ack) }
            },
        )
    })
    .await;

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

/// Stale h1 headers-read timer must not kill a WebSocket connection.
/// The h1 dispatcher starts a timer that can fire after the WS upgrade,
/// setting DSP_TIMEOUT on the shared IO. If the WS service is busy when
/// this happens, the dispatcher must ignore the spurious timeout.
#[ntex::test]
async fn test_stale_timer_after_ws_upgrade() {
    use ntex::util::inflight::InFlightService;

    async fn slow_ws_service(
        msg: DispatchItem<ws::Codec>,
    ) -> Result<Option<ws::Message>, io::Error> {
        match msg {
            DispatchItem::Item(frame) => {
                // sleep longer than the h1 headers_read_rate timer (1s)
                // so the stale timer fires while this service is busy
                sleep(Millis(2000)).await;
                let msg = match frame {
                    ws::Frame::Text(text) => {
                        ws::Message::Text(String::from_utf8_lossy(&text).as_ref().into())
                    }
                    ws::Frame::Close(reason) => ws::Message::Close(reason),
                    _ => return Ok(None),
                };
                Ok(Some(msg))
            }
            _ => Ok(None),
        }
    }

    let srv =
        test::server_with_config(
            async move || {
                HttpService::h1(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
                    .control(move |req: h1::Control<_, _>| {
                        let ack = if let h1::Control::Upgrade(upg) = req {
                            upg.handle(|req, io, codec| async move {
                                let res = handshake(req.head()).unwrap().message_body(());
                                io.encode((res, body::BodySize::None).into(), &codec)
                                    .unwrap();
                                io.set_config(SharedCfg::new("WS").add(
                                    IoConfig::new().set_keepalive_timeout(Seconds(0)),
                                ));
                                // let the stale h1 timer (1s) fire before starting the WS dispatcher
                                sleep(Millis(2500)).await;
                                // InFlightService(1) makes poll_ready return Pending while
                                // slow_ws_service is processing, so the dispatcher enters
                                // poll_service → poll_read_pause where DSP_TIMEOUT is observed
                                Dispatcher::new(
                                    io.seal(),
                                    ws::Codec::new(),
                                    InFlightService::new(
                                        1,
                                        ntex::service::fn_service(slow_ws_service),
                                    ),
                                )
                                .await
                                .map_err(|_| panic!())
                            })
                        } else {
                            req.ack()
                        };
                        async move { Ok::<_, io::Error>(ack) }
                    })
            },
            SharedCfg::new("SRV").add(
                HttpServiceConfig::new()
                    .set_keepalive(1)
                    .set_headers_read_rate(Seconds(1), Seconds::ZERO, 16),
            ),
        )
        .await;

    let conn = srv.ws().await.unwrap();
    assert_eq!(conn.response().status(), StatusCode::SWITCHING_PROTOCOLS);

    let (io, codec, _) = conn.into_inner();

    // send a message immediately — the service will sleep 2s,
    // during which the stale h1 timer (1s) fires
    io.send(ws::Message::Text(ByteString::from_static("hello")), &codec)
        .await
        .unwrap();

    // connection must survive; we get the echo back after ~2s
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Text(Bytes::from_static(b"hello")));

    io.send(
        ws::Message::Close(Some(ws::CloseCode::Normal.into())),
        &codec,
    )
    .await
    .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Normal.into())));
}
