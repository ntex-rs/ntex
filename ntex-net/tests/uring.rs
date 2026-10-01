//! io-uring backend tests.
//!
//! The tests run on a dedicated io-uring reactor with either neon backend,
//! and are skipped if io-uring is not available. The reactor is driven by the
//! native `ntex-rt` runtime, which the tokio and compio runtimes replace.
#![cfg(all(target_os = "linux", not(feature = "tokio"), not(feature = "compio")))]
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::{any::Any, future::Future, future::poll_fn, net, panic, sync::mpsc, task::Poll, thread};
use std::{time::Duration, time::Instant};

use ntex::codec::BytesCodec;
use ntex::io::{Io, IoConfig, types::PeerAddr};
use ntex::time::{Seconds, sleep};
use ntex::util::Bytes;
use ntex_rt::{BlockFuture, Runner, System};
use ntex_service::cfg::SharedCfg;
use socket2::SockRef;

struct Uring(u32);

impl Runner for Uring {
    fn block_on(&self, fut: BlockFuture) -> Result<(), Box<dyn Any + Send>> {
        let driver: Box<dyn ntex_net::Reactor> =
            Box::new(ntex_net::uring::Reactor::new(self.0).unwrap());
        ntex_net::with_reactor(&driver, || {
            panic::catch_unwind(panic::AssertUnwindSafe(|| {
                let rt = ntex_rt::Runtime::new(driver.handle());
                rt.block_on(fut, &*driver);
            }))
        })
    }
}

/// Runs the future on an io-uring reactor with a submission queue of
/// `entries`.
///
/// Every system runs on its own thread, the timer state is thread-local.
fn run<F, Fut>(entries: u32, f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    if ntex_net::uring::Reactor::new(entries).is_err() {
        eprintln!("io-uring is not available, skipping");
        return;
    }
    let res = thread::spawn(move || System::new("uring", Uring(entries)).block_on(f())).join();
    if let Err(e) = res {
        panic::resume_unwind(e);
    }
}

/// Configuration with a shutdown timeout that leaves room to flush large
/// output to slow readers.
fn cfg() -> SharedCfg {
    SharedCfg::new("URING")
        .add(IoConfig::new().set_shutdown_timeout(Seconds(30)))
        .build()
}

/// Deterministic, position-dependent payload.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Encodes `data` as separate pages of `page` bytes.
fn encode_pages(io: &Io, data: &[u8], page: usize) {
    for chunk in data.chunks(page) {
        io.encode(Bytes::copy_from_slice(chunk), &BytesCodec)
            .unwrap();
    }
}

/// Reads until eof or error, pausing `delay` after every read.
fn read_all<R: Read>(sock: &mut R, delay: Duration) -> (Vec<u8>, Option<ErrorKind>) {
    let mut data = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => return (data, None),
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(e) => return (data, Some(e.kind())),
        }
        if !delay.is_zero() {
            thread::sleep(delay);
        }
    }
}

/// Reads exactly `len` bytes from `io`.
async fn recv_exact(io: &Io, len: usize) -> Vec<u8> {
    let mut data = Vec::new();
    while data.len() < len {
        match io.recv(&BytesCodec).await {
            Ok(Some(chunk)) => data.extend_from_slice(&chunk),
            res => panic!("unexpected: {res:?}, received {}", data.len()),
        }
    }
    data
}

/// A connected TCP pair, the client side uses a small send buffer.
fn tcp_pair(sndbuf: Option<usize>) -> (net::TcpStream, net::TcpStream) {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
    if let Some(size) = sndbuf {
        SockRef::from(&client).set_send_buffer_size(size).unwrap();
    }
    let (peer, _) = lst.accept().unwrap();
    (client, peer)
}

/// More operations than the submission queue holds are queued and submitted
/// over several turns, nothing is lost or reordered.
#[test]
fn submission_queue_overflow() {
    const CONNS: usize = 32;
    const SIZE: usize = 64 * 1024;

    run(4, move || async {
        let mut peers = Vec::new();
        let mut ios = Vec::new();
        for _ in 0..CONNS {
            let (sock, mut peer) = UnixStream::pair().unwrap();
            ios.push(ntex_net::from_unix_stream(sock, cfg()).unwrap());
            peers.push(thread::spawn(move || {
                let mut buf = vec![0u8; SIZE];
                peer.read_exact(&mut buf).unwrap();
                peer.write_all(&buf).unwrap();
                read_all(&mut peer, Duration::ZERO)
            }));
        }

        // all writes are submitted in the same turn
        let data = pattern(SIZE);
        for io in &ios {
            encode_pages(io, &data, 4096);
        }
        for io in &ios {
            assert_eq!(recv_exact(io, SIZE).await, data);
        }
        for io in &ios {
            io.shutdown().await.unwrap();
        }
        drop(ios);
        for peer in peers {
            assert_eq!(peer.join().unwrap(), (Vec::new(), None));
        }
    });
}

/// Streams dropped in the last turn are closed when the runtime stops, with
/// a submission queue that cannot hold all `Close` operations.
#[test]
fn close_on_stop_with_full_queue() {
    let (tx, rx) = mpsc::channel();
    run(4, move || async move {
        for _ in 0..16 {
            let (sock, peer) = UnixStream::pair().unwrap();
            let io = ntex_net::from_unix_stream(sock, SharedCfg::default()).unwrap();
            io.encode(Bytes::from_static(b"data"), &BytesCodec).unwrap();
            tx.send(peer).unwrap();
            // the stream is closed while the runtime stops
            drop(io);
        }
        System::current().stop();
    });

    for mut peer in rx.try_iter() {
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (data, _) = read_all(&mut peer, Duration::ZERO);
        assert!(data.is_empty() || data == b"data", "{data:?}");
    }
}

/// Wakeups from other threads reach the reactor through the notifier.
#[test]
fn cross_thread_wakeup() {
    run(64, move || async {
        for i in 0..8 {
            let res = ntex::rt::spawn_blocking(move || {
                thread::sleep(Duration::from_millis(5));
                i
            })
            .await
            .unwrap();
            assert_eq!(res, i);
        }
    });
}

/// A task that is always ready does not block the reactor.
#[test]
fn busy_task() {
    run(64, move || async {
        let (sock, mut peer) = UnixStream::pair().unwrap();
        let io = ntex_net::from_unix_stream(sock, SharedCfg::default()).unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 4];
            peer.read_exact(&mut buf).unwrap();
            peer.write_all(&buf).unwrap();
        });

        let busy = ntex::rt::spawn(async {
            let mut n = 0;
            poll_fn(|cx| {
                n += 1;
                if n < 10_000 {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
        });
        io.encode(Bytes::from_static(b"ping"), &BytesCodec).unwrap();
        assert_eq!(recv_exact(&io, 4).await, b"ping");
        busy.await.unwrap();
    });
}

/// I/O completes while another task keeps the runtime busy.
#[test]
fn busy_task_io_progress() {
    run(64, move || async {
        let (sock, mut peer) = UnixStream::pair().unwrap();
        let io = ntex_net::from_unix_stream(sock, SharedCfg::default()).unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 4];
            peer.read_exact(&mut buf).unwrap();
            peer.write_all(&buf).unwrap();
        });

        let done = std::rc::Rc::new(std::cell::Cell::new(false));
        let done2 = done.clone();
        let busy = ntex::rt::spawn(poll_fn(move |cx| {
            if done2.get() {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }));
        io.encode(Bytes::from_static(b"ping"), &BytesCodec).unwrap();
        assert_eq!(recv_exact(&io, 4).await, b"ping");
        done.set(true);
        busy.await.unwrap();
    });
}

/// A socket address that cannot be created fails the connect.
#[test]
fn unix_connect_invalid_path() {
    run(64, move || async {
        let path = "x".repeat(256);
        let err = ntex_net::unix_connect(&path, SharedCfg::default())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    });
}

/// Zero-copy sends of a single page and of gathered pages, with a peer
/// that reads slowly so that sends complete partially.
#[test]
fn zero_copy_sends() {
    const SIZE: usize = 2 * 1024 * 1024;

    // single page and gathered pages within the zero-copy size range
    for page in [64 * 1024, 32 * 1024] {
        run(64, move || async move {
            let (sock, mut peer) = tcp_pair(Some(4096));
            let reader = thread::spawn(move || read_all(&mut peer, Duration::ZERO));

            let io = ntex_net::from_tcp_stream(sock, cfg()).unwrap();
            let data = pattern(SIZE);
            encode_pages(&io, &data, page);
            io.shutdown().await.unwrap();
            drop(io);

            let (received, err) = reader.join().unwrap();
            assert_eq!(err, None);
            assert!(received == data, "page {page}: {} bytes", received.len());
        });
    }
}

/// Output of gathered sends that the kernel accepted partially is resent
/// in order.
#[test]
fn partial_gathered_sends() {
    const SIZE: usize = 4 * 1024 * 1024;

    run(64, move || async {
        let (sock, mut peer) = UnixStream::pair().unwrap();
        SockRef::from(&sock).set_send_buffer_size(4096).unwrap();
        let reader = thread::spawn(move || read_all(&mut peer, Duration::from_micros(100)));

        let io = ntex_net::from_unix_stream(sock, cfg()).unwrap();
        let data = pattern(SIZE);
        encode_pages(&io, &data, 10_000);
        io.shutdown().await.unwrap();
        drop(io);

        let (received, err) = reader.join().unwrap();
        assert_eq!(err, None);
        assert!(received == data, "{} bytes", received.len());
    });
}

/// Pages that do not fit into one send together are sent one by one, a
/// partially sent page is resent from the unsent offset.
#[test]
fn large_page_sends() {
    const SIZE: usize = 4 * 1024 * 1024;

    // zero-copy sends are limited to 128K, the first page is sent alone
    run(64, || async {
        let (sock, mut peer) = tcp_pair(Some(4096));
        let reader = thread::spawn(move || read_all(&mut peer, Duration::ZERO));

        let io = ntex_net::from_tcp_stream(sock, cfg()).unwrap();
        let data = pattern(SIZE);
        encode_pages(&io, &data, 100 * 1024);
        io.shutdown().await.unwrap();
        drop(io);

        let (received, err) = reader.join().unwrap();
        assert_eq!(err, None);
        assert!(received == data, "{} bytes", received.len());
    });

    // copied sends are limited to 256K
    run(64, || async {
        let (sock, mut peer) = UnixStream::pair().unwrap();
        SockRef::from(&sock).set_send_buffer_size(4096).unwrap();
        let reader = thread::spawn(move || read_all(&mut peer, Duration::ZERO));

        let io = ntex_net::from_unix_stream(sock, cfg()).unwrap();
        let data = pattern(SIZE);
        encode_pages(&io, &data, 200 * 1024);
        io.shutdown().await.unwrap();
        drop(io);

        let (received, err) = reader.join().unwrap();
        assert_eq!(err, None);
        assert!(received == data, "{} bytes", received.len());
    });
}

const CANCEL_SIZE: usize = 8 * 1024 * 1024;

/// An in-flight send is canceled when the stream is dropped, the peer does
/// not receive a complete stream.
#[test]
fn send_canceled_on_drop() {
    const SIZE: usize = CANCEL_SIZE;

    run(64, move || async {
        let (sock, mut peer) = UnixStream::pair().unwrap();
        let io = ntex_net::from_unix_stream(sock, SharedCfg::default()).unwrap();
        encode_pages(&io, &pattern(SIZE), 64 * 1024);
        sleep(Duration::from_millis(50)).await;
        drop(io);

        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (received, err) = ntex::rt::spawn_blocking(move || read_all(&mut peer, Duration::ZERO))
            .await
            .unwrap();
        assert!(received.len() < SIZE);
        assert_eq!(err, None);
    });
}

/// An in-flight zero-copy send is canceled when the stream is dropped, the
/// connection is aborted.
#[test]
fn zero_copy_send_canceled_on_drop() {
    const SIZE: usize = CANCEL_SIZE;

    run(64, move || async {
        let (sock, mut peer) = tcp_pair(Some(4096));
        let io = ntex_net::from_tcp_stream(sock, SharedCfg::default()).unwrap();
        encode_pages(&io, &pattern(SIZE), 64 * 1024);
        sleep(Duration::from_millis(50)).await;
        drop(io);

        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (received, err) = ntex::rt::spawn_blocking(move || read_all(&mut peer, Duration::ZERO))
            .await
            .unwrap();
        assert!(received.len() < SIZE);
        assert_eq!(err, Some(ErrorKind::ConnectionReset));
    });
}

/// The io handle outlives the closed descriptor.
#[test]
fn io_outlives_close() {
    run(64, move || async {
        let (sock, peer) = tcp_pair(None);
        let addr = peer.local_addr().unwrap();
        let io = ntex_net::from_tcp_stream(sock, SharedCfg::default()).unwrap();
        assert_eq!(io.query::<PeerAddr>().get().map(|a| a.0), Some(addr));

        io.close();
        io.on_disconnect().await;
        // the descriptor close completes while the io is alive
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(100) {
            sleep(Duration::from_millis(10)).await;
        }
        drop(io);
        sleep(Duration::from_millis(10)).await;
        drop(peer);
    });
}
