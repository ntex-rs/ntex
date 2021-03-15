use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::{cell::Cell, io, marker::PhantomData, pin::Pin};

use bytes::Bytes;
use bytestring::ByteString;
use futures::{future, Future, SinkExt, StreamExt};

use ntex::codec::{AsyncRead, AsyncWrite};
use ntex::framed::{DispatchItem, Dispatcher, State, Timer};
use ntex::http::{body, h1, test, ws::handshake, HttpService, Request, Response};
use ntex::service::{fn_factory, Service};
use ntex::ws;

struct WsService<T>(Arc<Mutex<Cell<bool>>>, PhantomData<T>);

impl<T> WsService<T> {
    fn new() -> Self {
        WsService(Arc::new(Mutex::new(Cell::new(false))), PhantomData)
    }

    fn set_polled(&self) {
        *self.0.lock().unwrap().get_mut() = true;
    }

    fn was_polled(&self) -> bool {
        self.0.lock().unwrap().get()
    }
}

impl<T> Clone for WsService<T> {
    fn clone(&self) -> Self {
        WsService(self.0.clone(), PhantomData)
    }
}

impl<T> Service for WsService<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Request = (Request, T, State, h1::Codec);
    type Response = ();
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), io::Error>>>>;

    fn poll_ready(&self, _ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.set_polled();
        Poll::Ready(Ok(()))
    }

    fn call(&self, (req, io, state, mut codec): Self::Request) -> Self::Future {
        let fut = async move {
            let res = handshake(req.head()).unwrap().message_body(());

            state
                .write()
                .encode((res, body::BodySize::None).into(), &mut codec)
                .unwrap();

            Dispatcher::new(io, ws::Codec::new(), state, service, Timer::default())
                .await
                .map_err(|_| panic!())
        };

        Box::pin(fut)
    }
}

async fn service(
    msg: DispatchItem<ws::Codec>,
) -> Result<Option<ws::Message>, io::Error> {
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
                    future::ok::<_, io::Error>(ws_service.clone())
                }))
                .h1(|_| future::ok::<_, io::Error>(Response::NotFound()))
                .tcp()
        }
    });

    // client service
    let mut framed = srv.ws().await.unwrap();
    framed
        .send(ws::Message::Text(ByteString::from_static("text")))
        .await
        .unwrap();
    let (item, mut framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Text(Bytes::from_static(b"text"))
    );

    framed
        .send(ws::Message::Binary("text".into()))
        .await
        .unwrap();
    let (item, mut framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Binary(Bytes::from_static(&b"text"[..]))
    );

    framed.send(ws::Message::Ping("text".into())).await.unwrap();
    let (item, mut framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Pong("text".to_string().into())
    );

    framed
        .send(ws::Message::Continuation(ws::Item::FirstText(
            "text".into(),
        )))
        .await
        .unwrap();
    let (item, mut framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::FirstText(Bytes::from_static(b"text")))
    );

    assert!(framed
        .send(ws::Message::Continuation(ws::Item::FirstText(
            "text".into()
        )))
        .await
        .is_err());
    assert!(framed
        .send(ws::Message::Continuation(ws::Item::FirstBinary(
            "text".into()
        )))
        .await
        .is_err());

    framed
        .send(ws::Message::Continuation(ws::Item::Continue("text".into())))
        .await
        .unwrap();
    let (item, mut framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Continue(Bytes::from_static(b"text")))
    );

    framed
        .send(ws::Message::Continuation(ws::Item::Last("text".into())))
        .await
        .unwrap();
    let (item, mut framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Last(Bytes::from_static(b"text")))
    );

    assert!(framed
        .send(ws::Message::Continuation(ws::Item::Continue("text".into())))
        .await
        .is_err());

    assert!(framed
        .send(ws::Message::Continuation(ws::Item::Last("text".into())))
        .await
        .is_err());

    framed
        .send(ws::Message::Continuation(ws::Item::FirstBinary(
            Bytes::from_static(b"bin"),
        )))
        .await
        .unwrap();
    let (item, mut framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::FirstBinary(Bytes::from_static(b"bin")))
    );

    framed
        .send(ws::Message::Continuation(ws::Item::Continue("text".into())))
        .await
        .unwrap();
    let (item, mut framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Continue(Bytes::from_static(b"text")))
    );

    framed
        .send(ws::Message::Continuation(ws::Item::Last("text".into())))
        .await
        .unwrap();
    let (item, mut framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Continuation(ws::Item::Last(Bytes::from_static(b"text")))
    );

    framed
        .send(ws::Message::Close(Some(ws::CloseCode::Normal.into())))
        .await
        .unwrap();

    let (item, _framed) = framed.into_future().await;
    assert_eq!(
        item.unwrap().unwrap(),
        ws::Frame::Close(Some(ws::CloseCode::Normal.into()))
    );

    assert!(ws_service.was_polled());
}
