//! TCP listener integration tests.
use std::{io::Read, io::Write, net, thread, time::Duration};

use ntex::time::sleep;
use ntex_net::tcp_listener;

fn bind() -> net::TcpListener {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    lst.set_nonblocking(true).unwrap();
    lst
}

fn supported() -> bool {
    !cfg!(any(feature = "tokio", feature = "compio", windows))
}

/// STATEMENT: `accept` returns the connected stream and the peer address.
///
/// HOW:
/// - Register a non-blocking listener and connect a client from a thread.
/// - Accept and confirm the returned address equals the client's local
///   address.
/// - Write through the accepted stream and confirm the client reads it.
#[ntex::test]
async fn accept_returns_stream_and_peer_addr() {
    if !supported() {
        return;
    }
    let mut lst = tcp_listener(bind()).unwrap();
    let addr = lst.local_addr().unwrap();

    let client = thread::spawn(move || {
        let mut sock = net::TcpStream::connect(addr).unwrap();
        let mut buf = [0u8; 5];
        sock.read_exact(&mut buf).unwrap();
        (sock.local_addr().unwrap(), buf)
    });

    let (mut stream, peer) = lst.accept().await.unwrap();
    stream.set_nonblocking(false).unwrap();
    stream.write_all(b"hello").unwrap();

    let (local, buf) = client.join().unwrap();
    assert_eq!(peer, local);
    assert_eq!(&buf, b"hello");
}

/// STATEMENT: A pending `accept` wakes when a client connects later.
///
/// HOW:
/// - Start accepting with no client, so the call waits on the reactor.
/// - Connect from a thread after a delay.
/// - Confirm `accept` completes with that client's address, twice in a row
///   to cover rearming the registration.
#[ntex::test]
async fn pending_accept_wakes_on_connect() {
    if !supported() {
        return;
    }
    let mut lst = tcp_listener(bind()).unwrap();
    let addr = lst.local_addr().unwrap();

    for _ in 0..2 {
        let client = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let sock = net::TcpStream::connect(addr).unwrap();
            let local = sock.local_addr().unwrap();
            (sock, local)
        });

        let (_, peer) = lst.accept().await.unwrap();
        let (_sock, local) = client.join().unwrap();
        assert_eq!(peer, local);
    }
}

/// STATEMENT: Dropping a pending `accept` future does not lose a connection.
///
/// HOW:
/// - Poll `accept` until it waits, then drop the future.
/// - Connect a client and give the reactor time to process it.
/// - Call `accept` again and confirm it returns that client.
#[ntex::test]
async fn dropped_accept_future_keeps_connection() {
    if !supported() {
        return;
    }
    let mut lst = tcp_listener(bind()).unwrap();
    let addr = lst.local_addr().unwrap();

    let res = ntex::time::timeout(Duration::from_millis(50), lst.accept()).await;
    assert!(res.is_err(), "accepted without a client");

    let sock = net::TcpStream::connect(addr).unwrap();
    sleep(Duration::from_millis(50)).await;

    let (_, peer) = lst.accept().await.unwrap();
    assert_eq!(peer, sock.local_addr().unwrap());
}

/// STATEMENT: Dropping the listener deregisters and closes its socket.
///
/// HOW:
/// - Start accepting so the socket is registered, then drop the future and
///   the listener.
/// - Let the reactor run a turn.
/// - Confirm the address can be bound again and a new listener on it
///   accepts a client.
#[ntex::test]
async fn dropped_listener_releases_socket() {
    if !supported() {
        return;
    }
    let mut lst = tcp_listener(bind()).unwrap();
    let addr = lst.local_addr().unwrap();
    let res = ntex::time::timeout(Duration::from_millis(50), lst.accept()).await;
    assert!(res.is_err(), "accepted without a client");
    drop(lst);
    sleep(Duration::from_millis(50)).await;

    let sock = net::TcpListener::bind(addr).unwrap();
    sock.set_nonblocking(true).unwrap();
    let mut lst = tcp_listener(sock).unwrap();
    let client = net::TcpStream::connect(addr).unwrap();
    let (_, peer) = lst.accept().await.unwrap();
    assert_eq!(peer, client.local_addr().unwrap());
}

/// STATEMENT: Reactors without listener support return `Unsupported`.
///
/// HOW:
/// - Under the Tokio, Compio or IOCP reactor, register a listener.
/// - Confirm the error kind is `Unsupported`.
#[ntex::test]
async fn unsupported_reactor_returns_error() {
    if supported() {
        return;
    }
    let err = tcp_listener(bind()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}

/// STATEMENT: An OS error from accepting is returned to the caller, and the
/// listener formats with its local address.
///
/// HOW:
/// - Bind a non-blocking socket without calling `listen` and register it.
/// - Confirm `Debug` output contains the bound address.
/// - Confirm `accept` fails instead of waiting for a connection.
#[ntex::test]
async fn accept_on_unlistened_socket_fails() {
    if !supported() {
        return;
    }
    let addr: net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    sock.bind(&addr.into()).unwrap();
    sock.set_nonblocking(true).unwrap();

    let mut lst = tcp_listener(sock.into()).unwrap();
    let addr = lst.local_addr().unwrap();
    assert!(format!("{lst:?}").contains(&addr.to_string()));

    let res = ntex::time::timeout(Duration::from_secs(5), lst.accept()).await;
    assert!(res.expect("accept is not completed").is_err());
}
