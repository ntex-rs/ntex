use std::io;

use ntex::codec::BytesCodec;
use ntex::http::test::server as test_server;
use ntex::http::{body::BodySize, h1, HttpService, Response};
use ntex::io::{DispatchItem, Dispatcher, DispatcherConfig};
use ntex::service::{fn_factory_with_config, fn_service};
use ntex::web::{self, App, HttpRequest};
use ntex::ws::{self, handshake_response};
use ntex::{time::Seconds, util::ByteString, util::Bytes, util::Ready};

async fn ws_service(
    msg: DispatchItem<ws::Codec>,
) -> Result<Option<ws::Message>, io::Error> {
    let msg = match msg {
        DispatchItem::Item(msg) => match msg {
            ws::Frame::Ping(msg) => ws::Message::Pong(msg),
            ws::Frame::Text(text) => ws::Message::Text(
                String::from_utf8(Vec::from(text.as_ref())).unwrap().into(),
            ),
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
    let mut srv = test_server(|| {
        HttpService::build()
            .control(|req: h1::Control<_, _>| async move {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    upg.handle(|req, io, codec| async move {
                        let res = handshake_response(req.head()).finish();

                        // send handshake respone
                        io.encode(
                            h1::Message::Item((res.drop_body(), BodySize::None)),
                            &codec,
                        )
                        .unwrap();

                        // start websocket service
                        Dispatcher::new(
                            io.seal(),
                            ws::Codec::default(),
                            ws_service,
                            &Default::default(),
                        )
                        .await
                    })
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            })
            .finish(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
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
    let mut srv = test_server(|| {
        HttpService::build()
            .control(|req: h1::Control<_, _>| async move {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    upg.handle(|req, io, codec| async move {
                        let res = handshake_response(req.head()).finish();

                        // send handshake respone
                        io.encode(
                            h1::Message::Item((res.drop_body(), BodySize::None)),
                            &codec,
                        )
                        .unwrap();

                        // start websocket service
                        Dispatcher::new(
                            io.seal(),
                            ws::Codec::default(),
                            ws_service,
                            &Default::default(),
                        )
                        .await
                    })
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            })
            .finish(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
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
    let srv = test_server(|| {
        HttpService::build()
            .control(|req: h1::Control<_, _>| async move {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    upg.handle(|req, io, codec| async move {
                        let res = handshake_response(req.head()).finish();

                        // send handshake respone
                        io.encode(
                            h1::Message::Item((res.drop_body(), BodySize::None)),
                            &codec,
                        )
                        .unwrap();

                        // start websocket service
                        let cfg = DispatcherConfig::default();
                        cfg.set_keepalive_timeout(Seconds::ZERO);
                        Dispatcher::new(io.seal(), ws::Codec::default(), ws_service, &cfg)
                            .await
                    })
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            })
            .finish(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
    });

    // client service
    let con = ws::WsClient::build(srv.url("/"))
        .address(srv.addr())
        .timeout(Seconds(30))
        .keepalive_timeout(Seconds(1))
        .finish()
        .unwrap()
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
    async fn service(_: ws::Frame) -> Result<Option<ws::Message>, io::Error> {
        Ok(None)
    }

    let srv = test_server(|| {
        HttpService::build().finish(App::new().service(web::resource("/").route(web::to(
            |req: HttpRequest| async move {
                // some async context switch
                ntex::time::sleep(ntex::time::Seconds::ZERO).await;

                web::ws::start::<_, _, web::Error>(
                    req,
                    fn_factory_with_config(|_| async {
                        Ok::<_, web::Error>(fn_service(service))
                    }),
                )
                .await
            },
        ))))
    });

    let _ = ws::WsClient::build(srv.url("/"))
        .address(srv.addr())
        .timeout(Seconds(1))
        .keepalive_timeout(Seconds(1))
        .finish()
        .unwrap()
        .connect()
        .await
        .unwrap();
}
