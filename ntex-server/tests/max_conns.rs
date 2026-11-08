//! Connection limit tests.
use std::{net, time::Duration};

use ntex::codec::BytesCodec;
use ntex::time::{Millis, sleep, timeout};
use ntex::util::Bytes;
use ntex_io::Io;
use ntex_server::net::{TestServer, build_test_server};
use ntex_service::{cfg::SharedCfg, fn_service};

async fn echo(io: Io) -> Result<(), ()> {
    while let Ok(Some(msg)) = io.recv(&BytesCodec).await {
        if io.send(msg, &BytesCodec).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Starts a single-worker echo server, optionally with a connection limit.
fn echo_server(max_conns: Option<usize>) -> TestServer {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    build_test_server(ntex_server::NoConfig, async move |builder| {
        let builder = if let Some(num) = max_conns {
            builder.max_connections(num)
        } else {
            builder
        };
        builder
            .listen("test", lst, SharedCfg::default(), async |_: &()| {
                fn_service(echo)
            })
            .unwrap()
    })
    .set_addr(addr)
}

/// Sends `msg` and returns whether it is echoed back within `wait`.
async fn echoes(io: &Io, msg: &'static [u8], wait: Millis) -> bool {
    io.send(Bytes::from_static(msg), &BytesCodec).await.unwrap();
    matches!(timeout(wait, io.recv(&BytesCodec)).await, Ok(Ok(Some(m))) if m == msg)
}

/// STATEMENT: Connection limits belong to the server that configured them.
///
/// HOW:
/// - Create and drop a builder with `max_connections(0)` without running it.
/// - Start one server limited to one connection and one with the default limit.
/// - Open two connections to each server; the limited server echoes only the
///   first, while the default server echoes both.
#[ntex::test]
async fn test_max_connections_per_server() {
    drop(ntex_server::net::build().max_connections(0));

    let limited = echo_server(Some(1));
    let unlimited = echo_server(None);

    let l1 = limited.connect().await.unwrap();
    assert!(echoes(&l1, b"l1", Millis(3000)).await);
    let l2 = limited.connect().await.unwrap();
    assert!(!echoes(&l2, b"l2", Millis(300)).await);

    let u1 = unlimited.connect().await.unwrap();
    assert!(echoes(&u1, b"u1", Millis(3000)).await);
    let u2 = unlimited.connect().await.unwrap();
    assert!(echoes(&u2, b"u2", Millis(3000)).await);

    assert_eq!(limited.server().metrics().workers[0].active_connections, 1);
    assert_eq!(
        unlimited.server().metrics().workers[0].active_connections,
        2
    );
}

/// STATEMENT: A worker at its connection limit stops taking connections and resumes when one closes.
///
/// HOW:
/// - Start a one-worker echo server with `max_connections(1)` and open two connections.
/// - Check the second is not served, and worker and listener metrics show the pause.
/// - Close the first connection and check the second is then served and metrics recover.
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

    let m = srv.server().metrics();
    assert_eq!(m.workers.len(), 1);
    assert_eq!(m.workers[0].active_connections, 1);
    assert_eq!(m.workers[0].not_ready, 1);
    assert!(m.workers[0].not_ready_duration >= Duration::from_millis(300));
    assert!(m.accept_pauses >= 1);
    assert_eq!(m.listeners.len(), 1);
    assert_eq!(m.listeners[0].name, "test");
    assert_eq!(m.listeners[0].addr, addr.to_string());
    // io2 is either accepted or waits in the kernel backlog
    assert!((1..=2).contains(&m.listeners[0].accepted));

    // the second connection is served once the first one is closed
    io1.close();
    let res = timeout(Millis(3000), io2.recv(&BytesCodec)).await;
    assert_eq!(res.unwrap().unwrap().unwrap(), "2");

    let m = srv.server().metrics();
    assert_eq!(m.workers[0].items, 2);
    assert_eq!(m.workers[0].active_connections, 1);
    assert_eq!(m.listeners[0].accepted, 2);
    assert_eq!(m.accept_backlog, 0);
    io2.close();
    sleep(Millis(100)).await;
    let m = srv.server().metrics();
    assert_eq!(m.workers[0].active_connections, 0);
    // the accept loop runs again
    sleep(Millis(50)).await;
    let m2 = srv.server().metrics();
    assert_eq!(m.accept_paused_duration, m2.accept_paused_duration);
}
