//! Teardown of client connections created by `tcp_connect`.
//!
//! Client sockets take a separate path on some backends (on Windows they are
//! connected with `ConnectEx`, which leaves them only partially connected
//! unless the connect context is updated), so teardown has to be checked for
//! them as well.
use std::{io::ErrorKind, io::Read, net, thread, time::Duration};

use ntex_io::IoConfig;
use ntex_service::cfg::SharedCfg;

/// Must not fit in the socket buffers, so that output is still queued inside
/// the connection when it is torn down.
const PAYLOAD: usize = 64 * 1024 * 1024;

/// How long the peer waits before it starts reading, so output backs up.
const STALL: Duration = Duration::from_millis(400);

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Eof,
    Err(ErrorKind),
}

fn cfg() -> SharedCfg {
    SharedCfg::new("CLIENT")
        .add(IoConfig::new().set_read_buf(8192, 512, 16))
        .build()
}

/// Accepts one connection, stalls, then reads until it ends and reports the
/// byte count and how it ended. The read timeout keeps a connection that is
/// never closed from hanging the test; it shows up as `TimedOut`.
///
/// The timeout is set on the listener and inherited by the accepted socket.
/// Setting it on the accepted socket instead races the client: once the
/// connection has been reset, Linux rejects `setsockopt` with `EINVAL`.
fn peer() -> (net::SocketAddr, oneshot::Receiver<(usize, Outcome)>) {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    socket2::SockRef::from(&lst)
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let addr = lst.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    thread::spawn(move || {
        let (mut sock, _) = lst.accept().unwrap();
        thread::sleep(STALL);
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0;
        let outcome = loop {
            match sock.read(&mut buf) {
                Ok(0) => break Outcome::Eof,
                Ok(n) => total += n,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    break Outcome::Err(ErrorKind::TimedOut);
                }
                Err(err) => break Outcome::Err(err.kind()),
            }
        };
        let _ = tx.send((total, outcome));
    });
    (addr, rx)
}

/// A graceful close must deliver everything and end with a `FIN`.
#[ntex::test]
async fn client_graceful_close_delivers_and_closes() {
    let (addr, rx) = peer();
    let io = ntex_net::tcp_connect(addr, cfg()).await.unwrap();

    io.encode_slice(&vec![7u8; PAYLOAD]).unwrap();
    io.close();

    let (total, outcome) = rx.await.unwrap();
    assert_eq!(outcome, Outcome::Eof, "delivered {total} bytes");
    assert_eq!(total, PAYLOAD);
}

/// A force close must reset the connection, so the peer cannot mistake the
/// truncated stream for a complete one.
#[ntex::test]
async fn client_force_close_resets_connection() {
    let (addr, rx) = peer();
    let io = ntex_net::tcp_connect(addr, cfg()).await.unwrap();

    io.encode_slice(&vec![7u8; PAYLOAD]).unwrap();
    ntex::time::sleep(Duration::from_millis(100)).await;
    io.terminate();

    let (total, outcome) = rx.await.unwrap();
    assert_eq!(
        outcome,
        Outcome::Err(ErrorKind::ConnectionReset),
        "force close delivered {total} bytes and ended cleanly"
    );
    assert!(total < PAYLOAD);
}

/// Force-closing an idle connection must reset it too, not just one with
/// output still queued.
#[ntex::test]
async fn client_force_close_idle_resets_connection() {
    let (addr, rx) = peer();
    let io = ntex_net::tcp_connect(addr, cfg()).await.unwrap();

    ntex::time::sleep(Duration::from_millis(50)).await;
    io.terminate();

    let (total, outcome) = rx.await.unwrap();
    assert_eq!(outcome, Outcome::Err(ErrorKind::ConnectionReset));
    assert_eq!(total, 0);
}

/// Dropping `Io` with output it accepted but never delivered must reset the
/// connection.
#[ntex::test]
async fn client_drop_with_queued_output_resets_connection() {
    let (addr, rx) = peer();
    let io = ntex_net::tcp_connect(addr, cfg()).await.unwrap();

    io.encode_slice(&vec![7u8; PAYLOAD]).unwrap();
    ntex::time::sleep(Duration::from_millis(100)).await;
    drop(io);

    let (total, outcome) = rx.await.unwrap();
    assert_eq!(
        outcome,
        Outcome::Err(ErrorKind::ConnectionReset),
        "dropped io delivered {total} bytes and ended cleanly"
    );
}
