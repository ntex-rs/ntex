//! Connection reset by the peer.
use std::{io::ErrorKind, io::Write, net, thread, time::Duration};

use ntex::{codec::BytesCodec, util::Either};
use ntex_io::IoConfig;
use ntex_service::cfg::SharedCfg;

/// Resets the connection instead of closing it gracefully.
fn reset(sock: net::TcpStream) {
    socket2::SockRef::from(&sock)
        .set_linger(Some(Duration::ZERO))
        .unwrap();
    drop(sock);
}

/// A reset while the reader waits for input must end the read with an error,
/// not with a clean eof.
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
        res => panic!("expected a reset, got {res:?}"),
    }
}

/// A reset that arrives while reading is paused by backpressure must be
/// reported once reading resumes. Input received before the reset may be
/// discarded by the reset, but never more than was sent.
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
        .add(IoConfig::new().set_read_buf(1024, 256))
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
