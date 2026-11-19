//! Connection reset by the peer.
use std::{io::ErrorKind, io::Write, net, thread, time::Duration};

use ntex::{codec::BytesCodec, util::Either};
use ntex_bytes::BytePageSize;
use ntex_io::IoConfig;
use ntex_service::cfg::SharedCfg;

/// Resets the connection instead of closing it gracefully.
fn reset(sock: net::TcpStream) {
    socket2::SockRef::from(&sock)
        .set_linger(Some(Duration::ZERO))
        .unwrap();
    drop(sock);
}

/// STATEMENT: A reset while the reader waits for input ends the read with an error, not
/// a clean EOF.
///
/// HOW:
/// - The peer resets the connection 200 ms after accepting it.
/// - Expect `recv` to fail with `ConnectionReset` within 5 s.
/// - With compio on Windows, also accept `None`, which compio reports for a reset.
#[ntex::test]
async fn reset_while_waiting_for_input() {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (sock, _) = lst.accept().unwrap();
        thread::sleep(Duration::from_millis(200));
        reset(sock);
    });

    let io = ntex_net::tcp_connect(addr, SharedCfg::default())
        .await
        .unwrap();
    let res = ntex::time::timeout(ntex::time::Seconds(5), io.recv(&BytesCodec))
        .await
        .expect("reset not reported");
    peer.join().unwrap();
    match res {
        Err(Either::Right(err)) => assert_eq!(err.kind(), ErrorKind::ConnectionReset),
        // compio-driver maps `ERROR_NETNAME_DELETED`, a reset, to a 0-byte read
        Ok(None) if cfg!(all(windows, feature = "compio")) => (),
        res => panic!("expected a reset, got {res:?}"),
    }
}

/// STATEMENT: A reset that arrives while reading is paused by backpressure is reported
/// once reading resumes.
///
/// HOW:
/// - Use a 4 KiB read buffer and watermark; the peer sends 256 KiB, then resets.
/// - After 400 ms, receive until an error; expect `ConnectionReset`, not EOF.
/// - Expect no more than 256 KiB received before the error.
#[ntex::test]
async fn reset_while_read_paused() {
    const SENT: usize = 256 * 1024;

    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut sock, _) = lst.accept().unwrap();
        sock.write_all(&vec![b'x'; SENT]).unwrap();
        thread::sleep(Duration::from_millis(200));
        reset(sock);
    });

    let cfg = SharedCfg::new("RESET")
        .add(
            IoConfig::new()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity()),
        )
        .build();
    let io = ntex_net::tcp_connect(addr, cfg).await.unwrap();
    // let the read buffer fill up and the reset arrive
    ntex::time::sleep(Duration::from_millis(400)).await;
    peer.join().unwrap();

    let mut total = 0;
    let err = loop {
        match ntex::time::timeout(ntex::time::Seconds(5), io.recv(&BytesCodec))
            .await
            .expect("reset not reported")
        {
            Ok(Some(chunk)) => total += chunk.len(),
            Ok(None) => panic!("reset ended with a clean eof after {total} bytes"),
            Err(Either::Right(err)) => break err,
            Err(Either::Left(err)) => panic!("codec error {err:?}"),
        }
    };
    assert_eq!(err.kind(), ErrorKind::ConnectionReset);
    assert!(total <= SENT);
}
