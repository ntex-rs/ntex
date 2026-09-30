//! Connection limit test, in a separate binary because the limit is
//! process-wide.
use std::net;

use ntex::codec::BytesCodec;
use ntex::time::{Millis, timeout};
use ntex::util::Bytes;
use ntex_io::Io;
use ntex_server::net::build_test_server;
use ntex_service::{cfg::SharedCfg, fn_service};

async fn echo(io: Io) -> Result<(), ()> {
    while let Ok(Some(msg)) = io.recv(&BytesCodec).await {
        if io.send(msg, &BytesCodec).await.is_err() {
            break;
        }
    }
    Ok(())
}

#[ntex::test]
async fn test_max_connections() {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let srv = build_test_server(ntex_server::NoConfig, async move |builder| {
        builder
            .max_connections(1)
            .listen("test", lst, SharedCfg::default(), async |_: &()| {
                fn_service(echo)
            })
            .unwrap()
    })
    .set_addr(addr);

    let io1 = srv.connect().await.unwrap();
    io1.send(Bytes::from_static(b"1"), &BytesCodec)
        .await
        .unwrap();
    assert_eq!(io1.recv(&BytesCodec).await.unwrap().unwrap(), "1");

    // the worker is at the limit
    let io2 = srv.connect().await.unwrap();
    io2.send(Bytes::from_static(b"2"), &BytesCodec)
        .await
        .unwrap();
    assert!(timeout(Millis(300), io2.recv(&BytesCodec)).await.is_err());

    // the second connection is served once the first one is closed
    io1.close();
    let res = timeout(Millis(3000), io2.recv(&BytesCodec)).await;
    assert_eq!(res.unwrap().unwrap().unwrap(), "2");
}
