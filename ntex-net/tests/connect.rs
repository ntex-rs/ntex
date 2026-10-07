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

#[ntex::test]
async fn connect_host() {
    let addr = echo_peer();
    let io = connect::connect(format!("127.0.0.1:{}", addr.port()))
        .await
        .unwrap();
    assert_eq!(io.query::<PeerAddr>().get(), Some(PeerAddr(addr)));
    echo(&io).await;
}

#[ntex::test]
async fn connect_socket_addr() {
    // the address provides the socket address itself
    let addr = echo_peer();
    let io = connect::connect_with(addr, &SharedCfg::new("CLIENT").build())
        .await
        .unwrap();
    echo(&io).await;
}

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

#[ntex::test]
async fn connector_service() {
    let addr = echo_peer();
    let connector = Connector::<&str>::default();
    assert!(format!("{connector:?}").contains("Connector"));

    let svc = Pipeline::new(SharedCfg::default(), connector.clone());
    let io = svc
        .call(Connect::with("localhost", addr).set_port(addr.port()))
        .await
        .unwrap();
    echo(&io).await;
}

/// The first addresses that refuse the connection are skipped.
#[ntex::test]
async fn connect_falls_back_to_next_address() {
    let addr = echo_peer();
    let req = Connect::new("localhost").set_addrs([closed_addr(), closed_addr(), addr]);
    let io = connect::connect(req).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get(), Some(PeerAddr(addr)));
    echo(&io).await;
}

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

#[ntex::test]
async fn connect_resolver_error() {
    let err = connect::connect("nonexistent.invalid").await.unwrap_err();
    assert!(matches!(&*err, ConnectError::Resolver(_)), "{err:?}");
}

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

#[test]
#[should_panic(expected = "not in a ntex driver")]
fn helpers_require_a_driver() {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
    let _ = ntex_net::from_tcp_stream(sock, SharedCfg::default());
}

#[cfg(unix)]
#[ntex::test]
#[should_panic(expected = "reactor is already set")]
async fn nested_reactor() {
    let reactor: Box<dyn ntex_net::Reactor> = Box::new(ntex_net::polling::Reactor::new().unwrap());
    ntex_net::with_reactor(&reactor, || ());
}
