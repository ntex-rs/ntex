use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use actix_codec::BytesCodec;
use actix_rt::time::delay_for;
use actix_service::{service_fn, Service};
use actix_testing::TestServer;
use futures::future::ok;

use actix_ioframe::{Builder, Connect};

#[derive(Clone)]
struct State;

#[actix_rt::test]
async fn test_disconnect() -> std::io::Result<()> {
    let disconnect = Arc::new(AtomicBool::new(false));
    let disconnect1 = disconnect.clone();

    let srv = TestServer::with(move || {
        let disconnect1 = disconnect1.clone();

        Builder::new()
            .factory(service_fn(|conn: Connect<_>| {
                ok(conn.codec(BytesCodec).state(State))
            }))
            .disconnect(move |_, _| {
                disconnect1.store(true, Ordering::Relaxed);
            })
            .finish(service_fn(|_t| ok(None)))
    });

    let mut client = Builder::new()
        .service(|conn: Connect<_>| {
            let conn = conn.codec(BytesCodec).state(State);
            conn.sink().close();
            ok(conn)
        })
        .finish(service_fn(|_t| ok(None)));

    let conn = actix_connect::default_connector()
        .call(actix_connect::Connect::with(String::new(), srv.addr()))
        .await
        .unwrap();

    client.call(conn.into_parts().0).await.unwrap();
    let _ = delay_for(Duration::from_millis(100)).await;
    assert!(disconnect.load(Ordering::Relaxed));

    Ok(())
}
