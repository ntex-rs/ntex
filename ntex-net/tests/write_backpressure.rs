//! Regression tests for the write path under backpressure from the peer.
//!
//! These run against whichever reactor `ntex-net` selects by default, which on
//! Windows is the IOCP backend. There a send owns its pages until the kernel
//! completes it, and a completion may cover only part of them, so a peer that
//! reads slowly or not at all keeps sends in flight and forces partial ones.
//!
//! The invariants under test are that a stalled peer throttles the producer
//! instead of letting the write buffer grow, and that no byte is lost,
//! duplicated or reordered.

use std::{cell::Cell, io::Read, net, rc::Rc, sync::mpsc, thread, time::Duration};

use ntex::codec::BytesCodec;
use ntex_bytes::Bytes;
use ntex_io::IoConfig;
use ntex_service::cfg::SharedCfg;

const HIGH: usize = 64 * 1024;
const CHUNK: usize = 16 * 1024;

fn cfg() -> SharedCfg {
    SharedCfg::new("TEST")
        .add(IoConfig::new().set_write_buf(HIGH))
        .build()
}

/// Deterministic, position-dependent payload: any loss, duplication or
/// reordering shifts the sequence and is caught by the peer.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Spawns a blocking peer that waits for `go`, then reads until eof, pausing
/// every `pause_every` reads, and reports how many bytes matched the pattern.
fn peer(go: mpsc::Receiver<()>, pause_every: usize) -> (net::SocketAddr, mpsc::Receiver<usize>) {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    // inherited by the accepted socket, keeps the kernel from absorbing much
    let sock = socket2::SockRef::from(&lst);
    sock.set_recv_buffer_size(8 * 1024).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let addr = lst.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut sock, _) = lst.accept().unwrap();
        let _ = go.recv();
        let mut buf = vec![0u8; 4096];
        let mut seen = 0usize;
        let mut reads = 0usize;
        loop {
            let n = match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for (i, byte) in buf[..n].iter().enumerate() {
                assert_eq!(
                    *byte,
                    ((seen + i) % 251) as u8,
                    "payload diverged at offset {}",
                    seen + i
                );
            }
            seen += n;
            reads += 1;
            if pause_every != 0 && reads.is_multiple_of(pause_every) {
                thread::sleep(Duration::from_millis(1));
            }
        }
        let _ = tx.send(seen);
    });
    (addr, rx)
}

/// Writes `payload` in chunks, honouring write backpressure, and counts the
/// bytes handed to the io object.
async fn produce(io: ntex_io::Io, payload: Rc<Vec<u8>>, progress: Rc<Cell<usize>>) {
    for chunk in payload.chunks(CHUNK) {
        io.encode(Bytes::copy_from_slice(chunk), &BytesCodec)
            .unwrap();
        progress.set(progress.get() + chunk.len());
        io.flush(false).await.unwrap();
    }
    io.flush(true).await.unwrap();
    io.shutdown().await.unwrap();
}

/// A peer that stops reading must throttle the producer: sends stay in flight,
/// backpressure engages, and the producer stops handing over data. Once the
/// peer reads again, everything arrives in order.
#[ntex::test]
async fn stalled_peer_throttles_writer_and_loses_nothing() {
    const TOTAL: usize = 16 * 1024 * 1024;

    let (go, go_rx) = mpsc::channel();
    let (addr, done) = peer(go_rx, 0);
    let io = ntex_net::tcp_connect(addr, cfg()).await.unwrap();
    let ioref = io.get_ref();
    let progress = Rc::new(Cell::new(0));
    let producer = ntex::rt::spawn(produce(io, Rc::new(pattern(TOTAL)), progress.clone()));

    // let the kernel buffers fill up, then check that the producer is stuck
    ntex::time::sleep(Duration::from_millis(500)).await;
    let stalled_at = progress.get();
    ntex::time::sleep(Duration::from_millis(200)).await;
    assert!(
        stalled_at < TOTAL && progress.get() == stalled_at,
        "producer was not throttled: {stalled_at} then {} of {TOTAL}",
        progress.get()
    );
    assert!(ioref.is_wr_backpressure(), "write backpressure not engaged");

    go.send(()).unwrap();
    ntex::time::timeout(Duration::from_secs(30), producer)
        .await
        .expect("producer did not finish after the peer resumed")
        .unwrap();
    let seen = done.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(seen, TOTAL, "expected {TOTAL} bytes, peer got {seen}");
}

/// Inline pages keep their data inside the page itself, so a send may only
/// point into them once they sit where they stay until the send completes.
/// Without a send buffer every send of inline pages stays in flight, while the
/// peer is stalled and then while it reads slowly; no byte may be lost or
/// corrupted.
#[ntex::test]
async fn inline_pages_survive_pending_and_partial_sends() {
    const TOTAL: usize = 256 * 1024;
    const SMALL: usize = 13;
    const BATCH: usize = 256;

    let (go, go_rx) = mpsc::channel();
    let (addr, done) = peer(go_rx, 16);
    let sock = net::TcpStream::connect(addr).unwrap();
    // without a send buffer the kernel sends straight from the pages, so every
    // send stays in flight until the peer takes the data. Only Windows accepts
    // zero, BSDs reject it with `EINVAL` and Linux raises it to its minimum
    let sndbuf = if cfg!(windows) { 0 } else { 4096 };
    socket2::SockRef::from(&sock)
        .set_send_buffer_size(sndbuf)
        .unwrap();
    let io = ntex_net::from_tcp_stream(sock, cfg()).unwrap();
    let payload = pattern(TOTAL);

    let producer = ntex::rt::spawn(async move {
        for batch in payload.chunks(SMALL * BATCH) {
            for chunk in batch.chunks(SMALL) {
                let page = Bytes::copy_from_slice(chunk);
                assert!(page.is_inline());
                io.encode_bytes(page).unwrap();
            }
            io.flush(false).await.unwrap();
        }
        io.flush(true).await.unwrap();
        io.shutdown().await.unwrap();
    });

    // the kernel buffers fill up and sends of inline pages stay in flight
    ntex::time::sleep(Duration::from_millis(300)).await;
    go.send(()).unwrap();
    ntex::time::timeout(Duration::from_secs(60), producer)
        .await
        .expect("producer did not finish")
        .unwrap();
    let seen = done.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(seen, TOTAL, "expected {TOTAL} bytes, peer got {seen}");
}

/// A peer that reads in small pieces with pauses keeps sends partially
/// completing; no byte may be lost across them.
#[ntex::test]
async fn slow_peer_receives_everything_in_order() {
    const TOTAL: usize = 8 * 1024 * 1024;

    let (go, go_rx) = mpsc::channel();
    let (addr, done) = peer(go_rx, 16);
    go.send(()).unwrap();
    let io = ntex_net::tcp_connect(addr, cfg()).await.unwrap();

    ntex::time::timeout(
        Duration::from_secs(30),
        produce(io, Rc::new(pattern(TOTAL)), Rc::new(Cell::new(0))),
    )
    .await
    .expect("producer did not finish");
    let seen = done.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(seen, TOTAL, "expected {TOTAL} bytes, peer got {seen}");
}
