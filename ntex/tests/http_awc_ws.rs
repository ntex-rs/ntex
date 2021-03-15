use std::io;

use bytes::Bytes;
use bytestring::ByteString;
use futures::{future::ok, SinkExt, StreamExt};

use ntex::framed::{DispatchItem, Dispatcher, State};
use ntex::http::test::server as test_server;
use ntex::http::ws::handshake_response;
use ntex::http::{body::BodySize, h1, HttpService, Request, Response};
use ntex::rt::net::TcpStream;
use ntex::ws;

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
            .upgrade(
                |(req, io, state, mut codec): (Request, TcpStream, State, h1::Codec)| {
                    async move {
                        let res = handshake_response(req.head()).finish();

                        // send handshake respone
                        state
                            .write()
                            .encode(
                                h1::Message::Item((res.drop_body(), BodySize::None)),
                                &mut codec,
                            )
                            .unwrap();

                        // start websocket service
                        Dispatcher::new(
                            io,
                            ws::Codec::default(),
                            state,
                            ws_service,
                            Default::default(),
                        )
                        .await
                    }
                },
            )
            .finish(|_| ok::<_, io::Error>(Response::NotFound()))
            .tcp()
    });

    // client service
    let mut framed = srv.ws().await.unwrap();
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
