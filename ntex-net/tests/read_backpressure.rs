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

use ntex_bytes::BytePageSize;
use ntex_io::IoConfig;
use ntex_service::cfg::SharedCfg;

const PAGE: BytePageSize = BytePageSize::Size8;
const HIGH: usize = PAGE.capacity();

fn cfg(page: BytePageSize) -> SharedCfg {
    SharedCfg::new("TEST")
        .add(
            IoConfig::new()
                .set_read_size(page, page)
                .set_read_backpressure(page.capacity()),
        )
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

/// STATEMENT: Input that arrives while the reader is paused keeps the read buffer bounded
/// and is all delivered once reading resumes.
///
/// HOW:
/// - Connect to a peer that sends 8 MiB, read once and stop consuming for 500 ms.
/// - Expect read backpressure on and the buffer at most four times the watermark.
/// - Drain the rest; expect all data in order.
#[ntex::test]
async fn paused_reader_stays_bounded_and_loses_nothing() {
    const TOTAL: usize = 8 * 1024 * 1024;

    let addr = serve(pattern(TOTAL));
    let io = ntex_net::tcp_connect(addr, cfg(PAGE)).await.unwrap();

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

/// STATEMENT: Draining the read buffer releases backpressure, and input queued while
/// reading was paused is not lost.
///
/// HOW:
/// - Connect to a peer that sends 4 MiB, with an 8 KiB read watermark.
/// - Consume in 1 KiB pieces; expect all data in order.
#[ntex::test]
async fn backpressure_release_is_lossless() {
    const TOTAL: usize = 4 * 1024 * 1024;

    let addr = serve(pattern(TOTAL));
    let io = ntex_net::tcp_connect(addr, cfg(PAGE)).await.unwrap();

    // Consume in units far smaller than the watermark so backpressure engages
    // and releases many times over the transfer.
    drain_and_verify(&io, TOTAL, 1024).await;
}

/// STATEMENT: Repeatedly pausing and resuming reads loses no input.
///
/// HOW:
/// - Connect to a peer that sends 4 MiB, with an 8 KiB read watermark.
/// - Take the whole read buffer on each pass and pause 1 ms every 8 passes.
/// - Expect all data in order.
#[ntex::test]
async fn read_pause_churn_is_lossless() {
    const TOTAL: usize = 4 * 1024 * 1024;

    let addr = serve(pattern(TOTAL));
    let io = ntex_net::tcp_connect(addr, cfg(PAGE)).await.unwrap();

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
        if rounds.is_multiple_of(8) {
            ntex::time::sleep(Duration::from_millis(1)).await;
        }
    }
    assert_eq!(seen, TOTAL, "expected {TOTAL} bytes, observed {seen}");
}

/// STATEMENT: Input that arrives while part of the previous read is still unconsumed is
/// appended after that remainder, so the stream stays in order.
///
/// HOW:
/// - Connect to a peer that sends 2 MiB, with a 256 KiB read size and watermark.
/// - Consume only one 333-byte piece per `read_more`, leaving the rest buffered.
/// - Drain the buffer after EOF; expect all data in order and at least one
///   transport read merged into a non-empty remainder.
#[ntex::test]
async fn unconsumed_buffer_merge_preserves_order() {
    const TOTAL: usize = 2 * 1024 * 1024;
    const CHUNK: usize = 333;

    let addr = serve(pattern(TOTAL));
    // The remainder grows past the watermark as the transfer proceeds; each
    // `read_more` releases backpressure, so reads keep landing behind it.
    let io = ntex_net::tcp_connect(addr, cfg(BytePageSize::Size256))
        .await
        .unwrap();

    let verify = |got: &[u8], seen: usize| {
        for (i, byte) in got.iter().enumerate() {
            assert_eq!(
                *byte,
                ((seen + i) % 251) as u8,
                "payload diverged at offset {}",
                seen + i
            );
        }
    };

    let mut seen = 0usize;
    let mut left = 0usize;
    let mut merges = 0usize;
    // Consuming clears read readiness, so every `Some` after the first reports
    // a new transport read that landed behind the `left` bytes still buffered.
    while io.read_more().await.unwrap().is_some() {
        if left > 0 {
            merges += 1;
        }
        let got = io.with_read_dst(|b| {
            let got = b.split_to(std::cmp::min(CHUNK, b.len()));
            left = b.len();
            got
        });
        verify(&got, seen);
        seen += got.len();
    }

    let rest = io.with_read_dst(|b| b.take());
    verify(&rest, seen);
    seen += rest.len();

    assert_eq!(seen, TOTAL, "expected {TOTAL} bytes, observed {seen}");
    assert!(merges > 0, "no transport read was merged into a remainder");
}
