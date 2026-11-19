//! Connector and connection helper integration tests.
use std::{io::Read, io::Write, net, thread};

use ntex::{codec::BytesCodec, util::Bytes};
use ntex_io::types::PeerAddr;
use ntex_net::connect::{self, Connect, ConnectError, Connector};
use ntex_service::{Pipeline, cfg::SharedCfg};

/// Accepts one connection and echoes a single read back.
fn echo_peer() -> net::SocketAddr {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    thread::spawn(move || {
        let (mut sock, _) = lst.accept().unwrap();
        let mut buf = [0u8; 64];
        let n = sock.read(&mut buf).unwrap();
        sock.write_all(&buf[..n]).unwrap();
    });
    addr
}

/// An address with nothing listening on it.
fn closed_addr() -> net::SocketAddr {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    lst.local_addr().unwrap()
}

async fn echo(io: &ntex_io::Io) {
    io.send(Bytes::from_static(b"hello"), &BytesCodec)
        .await
        .unwrap();
    assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), "hello");
}

/// STATEMENT: `connect` resolves a `host:port` string and connects to it.
///
/// HOW:
/// - Connect to `127.0.0.1:<port>` of an echoing peer.
/// - Expect `PeerAddr` to be the peer address and the echo to round-trip.
#[ntex::test]
async fn connect_host() {
    let addr = echo_peer();
    let io = connect::connect(format!("127.0.0.1:{}", addr.port()))
        .await
        .unwrap();
    assert_eq!(io.query::<PeerAddr>().get(), Some(PeerAddr(addr)));
    echo(&io).await;
}

/// STATEMENT: `connect_with` accepts a `SocketAddr` as the request.
///
/// HOW:
/// - Connect to an echoing peer's `SocketAddr` with a named config.
/// - Expect the echo to round-trip.
#[ntex::test]
async fn connect_socket_addr() {
    // the address provides the socket address itself
    let addr = echo_peer();
    let io = connect::connect_with(addr, &SharedCfg::new("CLIENT").build())
        .await
        .unwrap();
    echo(&io).await;
}

/// STATEMENT: An explicit address overrides the intrinsic address of a `SocketAddr` request.
///
/// HOW:
/// - Build a request for a closed `SocketAddr` and override it with a listening peer.
/// - Check `addrs()` reports only the override and the connection reaches that peer.
#[ntex::test]
async fn connect_override_socket_addr() {
    let addr = echo_peer();
    let msg = Connect::new(closed_addr()).set_addr(Some(addr));
    assert_eq!(msg.addrs().collect::<Vec<_>>(), vec![addr]);
    let io = connect::connect(msg).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get(), Some(PeerAddr(addr)));
    echo(&io).await;
}

/// STATEMENT: `tcp_connect` connects to an IPv6 address.
///
/// HOW:
/// - Skip when no IPv6 loopback is available.
/// - Connect to an echoing peer on `[::1]`.
/// - Expect `PeerAddr` to be the peer address and the echo to round-trip.
#[ntex::test]
async fn connect_ipv6() {
    let Ok(lst) = net::TcpListener::bind("[::1]:0") else {
        // no IPv6 loopback
        return;
    };
    let addr = lst.local_addr().unwrap();
    thread::spawn(move || {
        let (mut sock, _) = lst.accept().unwrap();
        let mut buf = [0u8; 64];
        let n = sock.read(&mut buf).unwrap();
        sock.write_all(&buf[..n]).unwrap();
    });

    let io = ntex_net::tcp_connect(addr, SharedCfg::default())
        .await
        .unwrap();
    assert_eq!(io.query::<PeerAddr>().get(), Some(PeerAddr(addr)));
    echo(&io).await;
}

/// STATEMENT: `Connector` used as a service connects to the request's address.
///
/// HOW:
/// - Call a `Connector` pipeline with a `localhost` request that carries the peer address
///   and port.
/// - Expect the echo to round-trip.
#[ntex::test]
async fn connector_service() {
    let addr = echo_peer();
    let connector = Connector::<&str>::default();

    let svc = Pipeline::new(SharedCfg::default(), connector.clone());
    let io = svc
        .call(Connect::with("localhost", addr).set_port(addr.port()))
        .await
        .unwrap();
    echo(&io).await;
}

/// STATEMENT: Addresses that refuse the connection are skipped in favor of the next one.
///
/// HOW:
/// - Connect with two closed addresses followed by an echoing peer.
/// - Expect `PeerAddr` to be the peer address and the echo to round-trip.
#[ntex::test]
async fn connect_falls_back_to_next_address() {
    let addr = echo_peer();
    let req = Connect::new("localhost").set_addrs([closed_addr(), closed_addr(), addr]);
    let io = connect::connect(req).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get(), Some(PeerAddr(addr)));
    echo(&io).await;
}

/// STATEMENT: A refused connection is reported as an I/O error.
///
/// HOW:
/// - Connect to a closed address; expect `ConnectError::Io` with `ConnectionRefused`.
/// - Connect with only closed addresses; expect `ConnectError::Io`.
/// - Call `tcp_connect` with a closed address; expect `ConnectionRefused`.
#[ntex::test]
async fn connect_refused() {
    let err = connect::connect(closed_addr()).await.unwrap_err();
    assert!(
        matches!(&*err, ConnectError::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused),
        "{err:?}"
    );

    let req = Connect::new("localhost").set_addrs([closed_addr(), closed_addr()]);
    let err = connect::connect(req).await.unwrap_err();
    assert!(matches!(&*err, ConnectError::Io(_)), "{err:?}");

    let err = ntex_net::tcp_connect(closed_addr(), SharedCfg::default())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
}

/// STATEMENT: A host that cannot be resolved is reported as a resolver error.
///
/// HOW:
/// - Connect to `nonexistent.invalid`.
/// - Expect `ConnectError::Resolver`.
#[ntex::test]
async fn connect_resolver_error() {
    let err = connect::connect("nonexistent.invalid").await.unwrap_err();
    assert!(matches!(&*err, ConnectError::Resolver(_)), "{err:?}");
}

/// STATEMENT: `unix_connect` connects to a Unix socket path and fails for a missing one.
///
/// HOW:
/// - Connect to a listening socket file; expect no `PeerAddr` and the echo to round-trip.
/// - Remove the file and connect again; expect `NotFound`, or `ConnectionRefused` on
///   Windows.
// tokio has no unix sockets on Windows
#[cfg(any(unix, not(feature = "tokio")))]
#[ntex::test]
async fn unix_connect() {
    use socket2::{Domain, SockAddr, Socket, Type};

    let dir = std::env::temp_dir().join(format!("ntex-net-uds-{}", std::process::id()));
    let _ = std::fs::remove_file(&dir);
    let lst = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
    lst.bind(&SockAddr::unix(&dir).unwrap()).unwrap();
    lst.listen(1).unwrap();
    thread::spawn(move || {
        let (sock, _) = lst.accept().unwrap();
        let mut buf = [0u8; 64];
        let n = (&sock).read(&mut buf).unwrap();
        (&sock).write_all(&buf[..n]).unwrap();
    });

    let io = ntex_net::unix_connect(&dir, SharedCfg::default())
        .await
        .unwrap();
    // unix sockets have no peer socket address
    assert!(io.query::<PeerAddr>().get().is_none());
    echo(&io).await;
    let _ = std::fs::remove_file(&dir);

    let err = ntex_net::unix_connect(&dir, SharedCfg::default())
        .await
        .unwrap_err();
    let expected = if cfg!(windows) {
        // WinSock reports WSAECONNREFUSED for a missing socket file
        std::io::ErrorKind::ConnectionRefused
    } else {
        std::io::ErrorKind::NotFound
    };
    assert_eq!(err.kind(), expected, "{err:?}");
}

/// STATEMENT: `from_unix_stream` registers a connected Unix stream with the runtime.
///
/// HOW:
/// - Register one end of a `UnixStream` pair and echo on the other.
/// - Expect the echo to round-trip.
#[cfg(unix)]
#[ntex::test]
async fn from_unix_stream() {
    let (sock, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let io = ntex_net::from_unix_stream(sock, SharedCfg::default()).unwrap();
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        let n = peer.read(&mut buf).unwrap();
        peer.write_all(&buf[..n]).unwrap();
    });
    echo(&io).await;
}

/// STATEMENT: Registering a stream outside a runtime panics.
///
/// HOW:
/// - Call `from_tcp_stream` on a plain thread.
/// - Expect a panic with "not in a ntex driver".
#[test]
#[should_panic(expected = "not in a ntex driver")]
fn helpers_require_a_driver() {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
    let _ = ntex_net::from_tcp_stream(sock, SharedCfg::default());
}

/// STATEMENT: A reactor cannot be installed while another is set.
///
/// HOW:
/// - Inside a runtime, call `with_reactor` with a new reactor.
/// - Expect a panic with "reactor is already set".
#[cfg(any(unix, windows))]
#[ntex::test]
#[should_panic(expected = "reactor is already set")]
async fn nested_reactor() {
    #[cfg(unix)]
    let reactor: Box<dyn ntex_net::Reactor> = Box::new(ntex_net::polling::Reactor::new().unwrap());
    #[cfg(windows)]
    let reactor: Box<dyn ntex_net::Reactor> = Box::new(ntex_net::iocp::Reactor::new().unwrap());
    ntex_net::with_reactor(&reactor, || ());
}

/// Returns `TCP_NODELAY` of the open socket connecting `local` to `peer`.
#[cfg(unix)]
fn nodelay_of(local: net::SocketAddr, peer: net::SocketAddr) -> bool {
    use std::os::fd::BorrowedFd;

    for fd in 3..4096 {
        // SAFETY: the borrow only queries the socket, fds that are not open
        // or not sockets fail the lookups
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let sock = socket2::SockRef::from(&fd);
        if sock.local_addr().ok().and_then(|a| a.as_socket()) == Some(local)
            && sock.peer_addr().ok().and_then(|a| a.as_socket()) == Some(peer)
        {
            return sock.tcp_nodelay().unwrap();
        }
    }
    panic!("no socket for {local} -> {peer}");
}

/// STATEMENT: Connected streams disable Nagle, while `from_tcp_stream` leaves
/// `TCP_NODELAY` as the caller set it.
///
/// HOW:
/// - Connect with `tcp_connect` and accept the peer on a plain listener.
/// - Convert the accepted stream with `from_tcp_stream`.
/// - Check the option is enabled on the connected socket and still disabled
///   on the accepted one.
#[cfg(unix)]
#[ntex::test]
async fn tcp_nodelay() {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();

    let client = ntex_net::tcp_connect(addr, SharedCfg::default())
        .await
        .unwrap();
    let (sock, peer) = lst.accept().unwrap();
    assert!(!sock.nodelay().unwrap());
    let server = ntex_net::from_tcp_stream(sock, SharedCfg::default()).unwrap();

    assert!(nodelay_of(peer, addr), "connected stream");
    assert!(!nodelay_of(addr, peer), "accepted stream");
    drop((client, server));
}
