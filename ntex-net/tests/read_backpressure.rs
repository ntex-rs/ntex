//! Regression tests for the read path and read backpressure.
//!
//! These run against whichever reactor `ntex-net` selects by default, which on
//! Windows is the IOCP backend. That backend is the only one that hands a read
//! buffer to the kernel and then has to reclaim it when the application pauses
//! reading, so the pause/resume churn below is the interesting case: every
//! pause cancels an in-flight `WSARecv` and every resume submits a new one.
//!
//! The invariant under test is that none of that churn may lose, duplicate or
//! reorder a single byte.

use std::{io::Write, net, thread, time::Duration};

use ntex_io::IoConfig;
use ntex_service::cfg::SharedCfg;

const HIGH: usize = 8 * 1024;
const LOW: usize = 512;

fn cfg(high: usize, low: usize) -> SharedCfg {
    SharedCfg::new("TEST")
        .add(IoConfig::new().set_read_buf(high, low, 16))
        .build()
}

/// Deterministic, position-dependent payload: any loss, duplication or
/// reordering shifts the sequence and is caught by the consumer.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Spawns a blocking peer that writes `payload` and then closes.
fn serve(payload: Vec<u8>) -> net::SocketAddr {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    thread::spawn(move || {
        let (mut sock, _) = lst.accept().unwrap();
        let _ = sock.write_all(&payload);
        let _ = sock.flush();
    });
    addr
}

/// Reads exactly `len` bytes and verifies them against `pattern`, consuming
/// at most `chunk` bytes per pass so the transport is forced to suspend and
/// resume repeatedly.
async fn drain_and_verify(io: &ntex_io::Io, len: usize, chunk: usize) {
    let mut seen = 0usize;
    while seen < len {
        if io.read_more().await.unwrap().is_none() {
            break;
        }
        loop {
            let got = io.with_read_dst(|b| {
                let take = std::cmp::min(chunk, b.len());
                if take == 0 { None } else { Some(b.split_to(take)) }
            });
            let Some(got) = got else { break };
            for (i, byte) in got.iter().enumerate() {
                assert_eq!(
                    *byte,
                    ((seen + i) % 251) as u8,
                    "payload diverged at offset {}",
                    seen + i
                );
            }
            seen += got.len();
        }
    }
    assert_eq!(seen, len, "expected {len} bytes, observed {seen}");
}

/// A consumer that stops reading must not lose data that arrives while it is
/// paused: the application-facing buffer stays bounded at the watermark, and
/// everything the peer sent is still delivered once reading resumes.
///
/// This deliberately does *not* assert that the peer blocks. On Windows
/// loopback the kernel absorbs many megabytes regardless of what the
/// application does, so TCP-level throttling is not observable here. What is
/// observable, and what matters, is that the application buffer stays bounded
/// while paused and that nothing is dropped.
#[ntex::test]
async fn paused_reader_stays_bounded_and_loses_nothing() {
    const TOTAL: usize = 8 * 1024 * 1024;

    let addr = serve(pattern(TOTAL));
    let io = ntex_net::tcp_connect(addr, cfg(HIGH, LOW)).await.unwrap();

    // Let the peer push as much as it can while nothing is consuming.
    io.read_more().await.unwrap();
    ntex::time::sleep(Duration::from_millis(500)).await;

    let buffered = io.with_read_dst(|b| b.len());
    assert!(
        io.is_rd_backpressure(),
        "backpressure should be engaged, buffered={buffered}"
    );
    // The watermark triggers backpressure, it is not a hard cap: the check runs
    // after a completed read is merged in, so the buffer can overshoot by up to
    // one transport read. What matters is that it stays bounded rather than
    // growing with the amount the peer offered.
    assert!(
        buffered <= 4 * HIGH,
        "buffer grew past a bounded overshoot: {buffered} (high watermark {HIGH})"
    );

    // Now drain: every byte the peer sent must still arrive, in order.
    drain_and_verify(&io, TOTAL, 4096).await;
}

/// Draining the buffer releases backpressure and the stream resumes without
/// losing anything queued while it was suspended.
#[ntex::test]
async fn backpressure_release_is_lossless() {
    const TOTAL: usize = 4 * 1024 * 1024;

    let addr = serve(pattern(TOTAL));
    let io = ntex_net::tcp_connect(addr, cfg(HIGH, LOW)).await.unwrap();

    // Consume in units far smaller than the watermark so backpressure engages
    // and releases many times over the transfer.
    drain_and_verify(&io, TOTAL, 1024).await;
}

/// Repeated suspend/resume cycles cancel and resubmit the in-flight receive
/// many times; no byte may be lost across the churn.
#[ntex::test]
async fn read_pause_churn_is_lossless() {
    const TOTAL: usize = 4 * 1024 * 1024;

    let addr = serve(pattern(TOTAL));
    let io = ntex_net::tcp_connect(addr, cfg(HIGH, LOW)).await.unwrap();

    let mut seen = 0usize;
    let mut rounds = 0usize;
    while seen < TOTAL {
        if io.read_more().await.unwrap().is_none() {
            break;
        }
        let got = io.with_read_dst(|b| b.take());
        for (i, byte) in got.iter().enumerate() {
            assert_eq!(
                *byte,
                ((seen + i) % 251) as u8,
                "payload diverged at offset {}",
                seen + i
            );
        }
        seen += got.len();
        rounds += 1;
        // Yielding between reads leaves the socket idle, so the reactor has to
        // tear down and re-arm the receive rather than keeping one pending.
        if rounds % 8 == 0 {
            ntex::time::sleep(Duration::from_millis(1)).await;
        }
    }
    assert_eq!(seen, TOTAL, "expected {TOTAL} bytes, observed {seen}");
}

/// Bytes left unconsumed in the application buffer must stay ahead of bytes
/// delivered later: the transport buffer is appended, never prepended.
#[ntex::test]
async fn unconsumed_buffer_merge_preserves_order() {
    const TOTAL: usize = 2 * 1024 * 1024;

    let addr = serve(pattern(TOTAL));
    // A generous watermark keeps backpressure out of the picture so this
    // isolates the merge of a partially consumed buffer with fresh data.
    let io = ntex_net::tcp_connect(addr, cfg(256 * 1024, 1024))
        .await
        .unwrap();

    // Consuming an odd, small amount per pass guarantees a remainder is always
    // carried across the next transport read.
    drain_and_verify(&io, TOTAL, 333).await;
}
