use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::future::ok;

use ntex::channel::mpsc;
use ntex::codec::BytesCodec;
use ntex::framed::{Builder, Connect, FactoryBuilder};
use ntex::rt::time::delay_for;
use ntex::server::test_server;
use ntex::{fn_factory_with_config, fn_service, IntoService, Service};

#[derive(Clone)]
struct State(Option<mpsc::Sender<Bytes>>);

#[ntex::test]
async fn test_basic() {
    let client_item = Rc::new(Cell::new(false));

    let srv = test_server(move || {
        FactoryBuilder::new(fn_service(|conn: Connect<_, _>| async move {
            delay_for(Duration::from_millis(50)).await;
            Ok(conn.codec(BytesCodec).state(State(None)))
        }))
        // echo
        .build(fn_service(|t: BytesMut| ok(Some(t.freeze()))))
    });

    let item = client_item.clone();
    let client = Builder::new(fn_service(move |conn: Connect<_, _>| async move {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Bytes::from_static(b"Hello"));
        Ok(conn.codec(BytesCodec).out(rx).state(State(Some(tx))))
    }))
    .build(fn_factory_with_config(move |cfg: State| {
        let item = item.clone();
        let cfg = RefCell::new(cfg);
        ok((move |t: BytesMut| {
            assert_eq!(t.freeze(), Bytes::from_static(b"Hello"));
            item.set(true);
            // drop Sender, which will close connection
            cfg.borrow_mut().0.take();
            ok::<_, ()>(None)
        })
        .into_service())
    }));

    let conn = ntex::connect::Connector::default()
        .call(ntex::connect::Connect::with(String::new(), srv.addr()))
        .await
        .unwrap();

    client.call(conn).await.unwrap();
    assert!(client_item.get());
}
