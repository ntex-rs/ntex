use std::io;

use ntex::codec::BytesCodec;
use ntex::http::test::server as test_server;
use ntex::http::{HttpService, Response, body::BodySize, h1};
use ntex::io::{DispatchItem, Dispatcher, IoConfig};
use ntex::service::{Pipeline, cfg::SharedCfg};
use ntex::web::{self, App, HttpRequest};
use ntex::ws::{self, handshake_response};
use ntex::{time::Seconds, util::ByteString, util::Bytes};

async fn ws_service(msg: DispatchItem<ws::Codec>) -> Result<Option<ws::Message>, io::Error> {
    let msg = match msg {
        DispatchItem::Item(msg) => match msg {
            ws::Frame::Ping(msg) => ws::Message::Pong(msg),
            ws::Frame::Text(text) => {
                ws::Message::Text(String::from_utf8(Vec::from(text.as_ref())).unwrap().into())
            }
            ws::Frame::Binary(bin) => ws::Message::Binary(bin),
            ws::Frame::Close(reason) => ws::Message::Close(reason),
            _ => ws::Message::Close(None),
        },
        _ => return Ok(None),
    };
    Ok(Some(msg))
}

#[ntex::test]
async fn test_simple() {
    let srv = test_server(async |_| {
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();

                    // send handshake respone
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    // start websocket service
                    let _ = Dispatcher::new(
                        io.seal(),
                        ws::Codec::default(),
                        Pipeline::new((), ws_service),
                    )
                    .await;
                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
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
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Normal.into())));
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
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    // start websocket service
                    let _ = Dispatcher::new(
                        io.seal(),
                        ws::Codec::default(),
                        Pipeline::new((), ws_service),
                    )
                    .await;

                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    // client service
    let io = srv.ws().await.unwrap().into_transport();

    io.send(Bytes::from_static(b"text"), &BytesCodec)
        .await
        .unwrap();
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"text"));
}

#[ntex::test]
async fn test_keepalive_timeout() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::NotFound())).control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();

                    // send handshake respone
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    // start websocket service
                    // SAFETY: no reference returned by `io.cfg()` is retained
                    // across the protocol transition.
                    unsafe {
                        io.set_config(
                            SharedCfg::new("WS-SRV")
                                .add(IoConfig::new().set_keepalive_timeout(Seconds::ONE)),
                        );
                    }
                    let _ = Dispatcher::new(
                        io.seal(),
                        ws::Codec::default(),
                        Pipeline::new((), ws_service),
                    )
                    .await;

                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    // client service
    let con = ws::WsClient::new(
        srv.url("/"),
        ws::WsClientConfig::new()
            .set_address(srv.addr())
            .set_handshake_timeout(Seconds(30)),
    )
    .connect()
    .await
    .unwrap()
    .seal();
    let tx = con.sink();
    let rx = con.receiver();

    tx.send(ws::Message::Binary(Bytes::from_static(b"text")))
        .await
        .unwrap();
    let item = rx.recv().await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    let item = rx.recv().await;
    assert!(item.is_none());
}

#[ntex::test]
async fn test_upgrade_handler_with_await() {
    async fn ws_service(_: ws::Frame) -> Result<Option<ws::Message>, io::Error> {
        Ok(None)
    }

    let srv = test_server(async |_| {
        HttpService::new(App::new().service(web::resource("/").route(web::to(
            async move |req: HttpRequest| {
                // some async context switch
                ntex::time::sleep(ntex::time::Seconds::ZERO).await;
                let _ = web::ws::start(&req, None, ws_service).await;
            },
        ))))
    });

    let _ = ws::WsClient::new(
        srv.url("/"),
        ws::WsClientConfig::new()
            .set_address(srv.addr())
            .set_handshake_timeout(Seconds(1)),
    )
    .connect()
    .await
    .unwrap();
}

#[ntex::test]
async fn test_receiver_drop_closes_connection() {
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let srv = test_server(async move |_| {
        let closed_tx = closed_tx.clone();
        HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
            async move |req: h1::Control<_, _>| {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    let (ack, io, req, codec) = upg.handle();
                    let res = handshake_response(req.head()).build();
                    io.encode(h1::Message::Item((res.drop_body(), BodySize::None)), &codec)
                        .unwrap();

                    let closed_tx = closed_tx.clone();
                    let _ = Dispatcher::new(
                        io.seal(),
                        ws::Codec::default(),
                        Pipeline::new((), async move |msg: DispatchItem<ws::Codec>| {
                            if let DispatchItem::Item(ws::Frame::Close(reason)) = &msg {
                                let _ = closed_tx.send(reason.clone());
                            }
                            ws_service(msg).await
                        }),
                    )
                    .await;
                    ack
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            },
        )
    });

    let con = ws::WsClient::new(
        srv.url("/"),
        ws::WsClientConfig::new()
            .set_address(srv.addr())
            .set_handshake_timeout(Seconds(30)),
    )
    .connect()
    .await
    .unwrap()
    .seal();
    let sink = con.sink();
    let rx = con.receiver();
    ntex::time::sleep(ntex::time::Millis(50)).await;
    assert!(sink.io().is_active());

    // the peer is idle, dropping the receiver still closes the connection
    drop(rx);
    ntex::time::timeout(Seconds(3), sink.on_disconnect())
        .await
        .expect("connection is not closed");
    assert_eq!(
        closed_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        Some(ws::CloseCode::Normal.into())
    );
}
