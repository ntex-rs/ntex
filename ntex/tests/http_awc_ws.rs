use std::io;

use ntex::http::test::server as test_server;
use ntex::http::ws::handshake_response;
use ntex::http::{body::BodySize, h1, HttpService, Request, Response};
use ntex::io::{DispatchItem, Dispatcher, Io};
use ntex::{util::ByteString, util::Bytes, util::Ready, ws};

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
            .upgrade(|(req, io, codec): (Request, Io, h1::Codec)| {
                async move {
                    let res = handshake_response(req.head()).finish();

                    // send handshake respone
                    io.encode(
                        h1::Message::Item((res.drop_body(), BodySize::None)),
                        &codec,
                    )
                    .unwrap();

                    // start websocket service
                    Dispatcher::new(
                        io.into_boxed(),
                        ws::Codec::default(),
                        ws_service,
                        Default::default(),
                    )
                    .await
                }
            })
            .finish(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
    });

    // client service
    let (_, io, codec) = srv.ws().await.unwrap().into_inner();
    io.send(ws::Message::Text(ByteString::from_static("text")), &codec)
        .await
        .unwrap();
    let item = io.next(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Text(Bytes::from_static(b"text")));

    io.send(ws::Message::Binary("text".into()), &codec)
        .await
        .unwrap();
    let item = io.next(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    io.send(ws::Message::Ping("text".into()), &codec)
        .await
        .unwrap();
    let item = io.next(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Pong("text".to_string().into()));

    io.send(
        ws::Message::Close(Some(ws::CloseCode::Normal.into())),
        &codec,
    )
    .await
    .unwrap();

    let item = io.next(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Normal.into())));
}
