use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use actix_codec::BytesCodec;
use actix_server_config::Io;
use actix_service::{new_apply_fn, Service};
use actix_testing::{self as test, TestServer};
use futures::Future;
use tokio_tcp::TcpStream;
use tokio_timer::sleep;

use actix_ioframe::{Builder, Connect};

struct State;

#[test]
fn test_disconnect() -> std::io::Result<()> {
    let disconnect = Arc::new(AtomicBool::new(false));
    let disconnect1 = disconnect.clone();

    let srv = TestServer::with(move || {
        let disconnect1 = disconnect1.clone();

        new_apply_fn(
            Builder::new()
                .factory(|conn: Connect<_>| Ok(conn.codec(BytesCodec).state(State)))
                .disconnect(move |_, _| {
                    disconnect1.store(true, Ordering::Relaxed);
                })
                .finish(|_t| Ok(None)),
            |io: Io<TcpStream>, srv| srv.call(io.into_parts().0),
        )
    });

    let mut client = Builder::new()
        .service(|conn: Connect<_>| {
            let conn = conn.codec(BytesCodec).state(State);
            conn.sink().close();
            Ok(conn)
        })
        .finish(|_t| Ok(None));

    let conn = test::block_on(
        actix_connect::default_connector()
            .call(actix_connect::Connect::with(String::new(), srv.addr())),
    )
    .unwrap();

    test::block_on(client.call(conn.into_parts().0)).unwrap();
    let _ = test::block_on(
        sleep(Duration::from_millis(100))
            .map(|_| ())
            .map_err(|_| ()),
    );
    assert!(disconnect.load(Ordering::Relaxed));

    Ok(())
}
