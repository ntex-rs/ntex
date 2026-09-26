//! Connection teardown integration tests.
use std::{io::ErrorKind, io::Read, io::Write, net, thread, time::Duration};

use ntex::{codec::BytesCodec, fn_service, io::Io, server::test_server, util::Bytes};

/// Must not fit in the socket send buffer, so that output is still queued when
/// the connection is torn down.
const PAYLOAD: usize = 8 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Eof,
    Err(ErrorKind),
}

/// Reads until the connection ends, returning the byte count and how it ended.
fn read_to_end(sock: &mut net::TcpStream) -> (usize, Outcome) {
    let mut total = 0;
    let mut buf = [0u8; 8192];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => return (total, Outcome::Eof),
            Ok(n) => total += n,
            Err(err) => return (total, Outcome::Err(err.kind())),
        }
    }
}

/// A force close must reset the connection.
///
/// The application discarded whatever was still buffered, so the peer must not
/// see the truncated stream end with a clean `FIN`: that is indistinguishable
/// from a complete response.
#[ntex::test]
async fn force_close_resets_connection() {
    let srv = test_server(async || {
        fn_service(|io: Io<_>| async move {
            let _ = io.recv(&BytesCodec).await;
            io.encode(Bytes::from(vec![b'y'; PAYLOAD]), &BytesCodec)
                .unwrap();
            io.terminate();
            ntex::time::sleep(Duration::from_millis(200)).await;
            Ok::<_, ()>(())
        })
    });

    let mut sock = net::TcpStream::connect(srv.addr()).unwrap();
    // set before the request: once the server has reset the connection,
    // Linux rejects `setsockopt` with `EINVAL`
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    sock.write_all(b"ping").unwrap();
    // let the connection be torn down before the receive queue is drained
    thread::sleep(Duration::from_millis(400));

    let (total, outcome) = read_to_end(&mut sock);
    assert_eq!(
        outcome,
        Outcome::Err(ErrorKind::ConnectionReset),
        "force close delivered {total} bytes and ended cleanly"
    );
    assert!(total < PAYLOAD);
}

/// A graceful shutdown must still deliver everything and close cleanly.
#[ntex::test]
async fn graceful_shutdown_closes_cleanly() {
    let srv = test_server(async || {
        fn_service(|io: Io<_>| async move {
            let _ = io.recv(&BytesCodec).await;
            io.encode(Bytes::from(vec![b'y'; PAYLOAD]), &BytesCodec)
                .unwrap();
            io.shutdown().await.unwrap();
            Ok::<_, ()>(())
        })
    });

    let mut sock = net::TcpStream::connect(srv.addr()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    sock.write_all(b"ping").unwrap();

    let (total, outcome) = read_to_end(&mut sock);
    assert_eq!(outcome, Outcome::Eof);
    assert_eq!(total, PAYLOAD);
}

/// Dropping the `Io` must reset the connection.
///
/// A service that returns without shutting down discards whatever it had
/// encoded, so the peer must not see that as a complete, empty response.
#[ntex::test]
async fn drop_resets_connection() {
    let srv = test_server(async || {
        fn_service(|io: Io<_>| async move {
            let _ = io.recv(&BytesCodec).await;
            io.encode(Bytes::from_static(b"pong"), &BytesCodec).unwrap();
            // the service returns, dropping `Io` without a graceful shutdown
            Ok::<_, ()>(())
        })
    });

    let mut sock = net::TcpStream::connect(srv.addr()).unwrap();
    // see `force_close_resets_connection`
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    sock.write_all(b"ping").unwrap();

    let (total, outcome) = read_to_end(&mut sock);
    assert_eq!(
        outcome,
        Outcome::Err(ErrorKind::ConnectionReset),
        "dropped io delivered {total} bytes and ended cleanly"
    );
}

/// A graceful shutdown must be bounded by the shutdown timeout when the peer
/// stops reading, even while a transport write is in flight.
#[ntex::test]
async fn shutdown_times_out_on_stalled_peer() {
    let (tx, rx) = std::sync::mpsc::channel();
    let srv = test_server(move || {
        let tx = tx.clone();
        async move {
            fn_service(move |io: Io<_>| {
                let tx = tx.clone();
                async move {
                    let _ = io.recv(&BytesCodec).await;
                    // copied into regular write pages: a single large page is
                    // one send, and Windows completes a send of any size at
                    // once while its send backlog is below `SO_SNDBUF`
                    io.encode_slice(&vec![b'y'; 4 * PAYLOAD]).unwrap();
                    let _ = tx.send(io.shutdown().await);
                    Ok::<_, ()>(())
                }
            })
        }
    });

    let mut sock = net::TcpStream::connect(srv.addr()).unwrap();
    sock.write_all(b"ping").unwrap();

    // the peer never reads, the default shutdown timeout is one second
    let res = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("shutdown did not complete");
    assert_eq!(res.unwrap_err().kind(), ErrorKind::TimedOut);
    drop(sock);
}
