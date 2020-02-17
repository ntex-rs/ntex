use std::cell::Cell;
use std::rc::Rc;

use actix_codec::BytesCodec;
use actix_service::{fn_factory_with_config, fn_service, IntoService, Service};
use actix_utils::mpsc;
use bytes::{Bytes, BytesMut};
use futures::future::ok;

use ntex::framed::{Builder, Connect, FactoryBuilder};
use ntex::server::test_server;

#[derive(Clone)]
struct State(Option<mpsc::Sender<Bytes>>);

#[actix_rt::test]
async fn test_basic() {
    let client_item = Rc::new(Cell::new(false));

    let srv = test_server(move || {
        FactoryBuilder::new(fn_service(|conn: Connect<_, _>| {
            ok(conn.codec(BytesCodec).state(State(None)))
        }))
        // echo
        .build(fn_service(|t: BytesMut| ok(Some(t.freeze()))))
    });

    let item = client_item.clone();
    let mut client = Builder::new(fn_service(move |conn: Connect<_, _>| async move {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Bytes::from_static(b"Hello"));
        Ok(conn.codec(BytesCodec).out(rx).state(State(Some(tx))))
    }))
    .build(fn_factory_with_config(move |mut cfg: State| {
        let item = item.clone();
        ok((move |t: BytesMut| {
            assert_eq!(t.freeze(), Bytes::from_static(b"Hello"));
            item.set(true);
            // drop Sender, which will close connection
            cfg.0.take();
            ok::<_, ()>(None)
        })
        .into_service())
    }));

    let conn = actix_connect::default_connector()
        .call(actix_connect::Connect::with(String::new(), srv.addr()))
        .await
        .unwrap();

    client.call(conn.into_parts().0).await.unwrap();
    assert!(client_item.get());
}
