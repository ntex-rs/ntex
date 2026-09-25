#![recursion_limit = "256"]
#![cfg(all(windows, feature = "openssl"))]

use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};

use ntex::client::Client;
use ntex::http::{HttpService, Uri, Version, openssl, test::server as test_server};
use ntex::service::{cfg::SharedCfg, service};
use ntex::web::{self, App, HttpResponse};
use ntex_tls::schannel::{ClientConfig, TlsConnector};
use tls_openssl::ssl::{AlpnError, SslAcceptor, SslFiletype, SslMethod};

fn ssl_acceptor() -> SslAcceptor {
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./tests/cert.pem")
        .unwrap();
    builder.set_alpn_select_callback(|_, protos| {
        const H2: &[u8] = b"\x02h2";
        if protos.windows(3).any(|window| window == H2) {
            Ok(b"h2")
        } else {
            Err(AlpnError::NOACK)
        }
    });
    builder.set_alpn_protos(b"\x02h2").unwrap();
    builder.build()
}

#[ntex::test]
async fn test_connection_reuse_h2() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(async move |_| {
        let num2 = num2.clone();
        service(async move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ok(io)
        })
        .and_then(openssl(
            ssl_acceptor(),
            HttpService::h2(
                App::new().service(web::resource("/").route(web::to(async || HttpResponse::Ok()))),
            ),
        ))
    });

    let tls = TlsConnector::<ntex::connect::Connector<Uri>>::with_config(
        ClientConfig::new().danger_accept_invalid_certs(true),
    );
    let client = Client::builder()
        .secure_connector(tls)
        .build(SharedCfg::default());

    let response = client.get(srv.surl("/")).send().await.unwrap();
    assert!(response.status().is_success());

    let response = client.post(srv.surl("/")).send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.version(), Version::HTTP_2);

    assert_eq!(num.load(Ordering::Relaxed), 1);
}

fn schannel_connector() -> TlsConnector<ntex::connect::Connector<&'static str>> {
    TlsConnector::with_config(ClientConfig::new().danger_accept_invalid_certs(true))
}

/// ALPN protocols offered by the client follow the configuration.
#[ntex::test]
async fn test_alpn_protocols() {
    use ntex::{connect::Connect, io::types::HttpProtocol, service::Pipeline};
    use std::sync::Mutex;

    async fn offered(config: ClientConfig) -> (Option<Vec<u8>>, HttpProtocol) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(None));
        let seen2 = seen.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
            builder
                .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
                .unwrap();
            builder
                .set_certificate_chain_file("./tests/cert.pem")
                .unwrap();
            builder.set_alpn_select_callback(move |_, protos| {
                *seen2.lock().unwrap() = Some(protos.to_vec());
                tls_openssl::ssl::select_next_proto(b"\x02h2\x08http/1.1", protos)
                    .ok_or(AlpnError::NOACK)
            });
            let (sock, _) = listener.accept().unwrap();
            let _stream = builder.build().accept(sock).unwrap();
            let _ = done_rx.recv();
        });

        let conn = Pipeline::new(
            SharedCfg::default(),
            TlsConnector::<ntex::connect::Connector<&'static str>>::with_config(
                config.danger_accept_invalid_certs(true),
            ),
        );
        let io = conn
            .call(Connect::new("localhost").set_addr(Some(addr)))
            .await
            .unwrap();
        let proto = io.query::<HttpProtocol>().get().unwrap();
        done_tx.send(()).unwrap();
        server.join().unwrap();
        let seen = seen.lock().unwrap().take();
        (seen, proto)
    }

    assert_eq!(
        offered(ClientConfig::new()).await,
        (Some(b"\x02h2\x08http/1.1".to_vec()), HttpProtocol::Http2)
    );
    assert_eq!(
        offered(ClientConfig::new().set_alpn_protocols(&["http/1.1"])).await,
        (Some(b"\x08http/1.1".to_vec()), HttpProtocol::Http1)
    );
    assert_eq!(
        offered(ClientConfig::new().set_alpn_protocols::<&str>(&[])).await,
        (None, HttpProtocol::Http1)
    );
}

/// A write page larger than one TLS record must be fully encrypted in a single
/// filter pass, not one record per transport write completion.
#[ntex::test]
async fn test_large_write_encrypted_in_one_pass() {
    use ntex::{codec::BytesCodec, connect::Connect, io::Io, server, service::Pipeline};

    const SIZE: usize = 16 * 1024 * 1024;

    let srv = server::test_server(async || {
        service(ntex::server::openssl::SslAcceptor::new(ssl_acceptor())).and_then(
            async move |io: Io<_>| {
                // let the client's socket send buffer fill up
                ntex::time::sleep(ntex::time::Millis(500)).await;

                let mut total = 0;
                while total < SIZE {
                    total += io.recv(&BytesCodec).await.unwrap().unwrap().len();
                }
                io.send(ntex::util::Bytes::from_static(b"done"), &BytesCodec)
                    .await
                    .unwrap();
                Ok::<_, std::io::Error>(())
            },
        )
    });

    let conn = Pipeline::new(SharedCfg::default(), schannel_connector());
    let io = conn
        .call(Connect::new("localhost").set_addr(Some(srv.addr())))
        .await
        .unwrap();

    // a single page larger than the maximum TLS record
    io.encode(ntex::util::Bytes::from(vec![b'x'; SIZE]), &BytesCodec)
        .unwrap();
    // let the write task run while the peer is not reading yet
    ntex::time::sleep(ntex::time::Millis(100)).await;
    let pending = io
        .with_buf(|buf| buf.with_write_buffers(|src, _| src.len()))
        .unwrap();
    assert_eq!(pending, 0, "plaintext left unencrypted after write");

    io.flush(true).await.unwrap();
    assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), "done");
}

/// Records must be encrypted in place into the transport's write pages.
#[ntex::test]
async fn test_records_fit_write_pages() {
    use ntex::{codec::BytesCodec, connect::Connect, service::Pipeline};
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    const SIZE: usize = 256 * 1024;

    #[derive(Debug)]
    struct Recorder(std::net::TcpStream, Arc<Mutex<Vec<u8>>>);

    impl Read for Recorder {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1.lock().unwrap().extend_from_slice(&buf[..n]);
            Ok(n)
        }
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let raw = Arc::new(Mutex::new(Vec::new()));
    let raw2 = raw.clone();
    let server = std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        let mut stream = ssl_acceptor().accept(Recorder(sock, raw2)).unwrap();
        stream
            .get_ref()
            .0
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut received = Vec::with_capacity(SIZE);
        while received.len() < SIZE {
            let n = stream.read(&mut buf).unwrap();
            received.extend_from_slice(&buf[..n]);
        }
        stream.write_all(b"done").unwrap();
        received
    });

    let conn = Pipeline::new(SharedCfg::default(), schannel_connector());
    let io = conn
        .call(Connect::new("localhost").set_addr(Some(addr)))
        .await
        .unwrap();
    let data: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    // small writes leave partially filled pages, then one large write
    let (small, large) = data.split_at(SIZE / 4);
    for chunk in small.chunks(997) {
        io.encode(ntex::util::Bytes::copy_from_slice(chunk), &BytesCodec)
            .unwrap();
        if chunk[0] % 4 == 0 {
            ntex::time::sleep(ntex::time::Millis(1)).await;
        }
    }
    io.encode(ntex::util::Bytes::copy_from_slice(large), &BytesCodec)
        .unwrap();
    let res = ntex::time::timeout(ntex::time::Millis(15_000), io.recv(&BytesCodec)).await;
    let received = server.join().unwrap();
    assert_eq!(res.unwrap().unwrap().unwrap(), "done");
    assert!(received == data, "plaintext corrupted");

    let raw = raw.lock().unwrap();
    let mut pos = 0;
    let mut records = Vec::new();
    while pos + 5 <= raw.len() {
        let len = u16::from_be_bytes([raw[pos + 3], raw[pos + 4]]) as usize;
        if raw[pos] == 23 {
            records.push(5 + len);
        }
        pos += 5 + len;
    }
    assert!(records.len() > SIZE / 16384, "{records:?}");
    let page = ntex::util::BytePageSize::Size16.capacity();
    assert!(
        records.iter().all(|len| *len <= page),
        "records larger than a write page: {records:?}"
    );
}

/// Graceful shutdown must send close_notify to the peer.
#[ntex::test]
async fn test_shutdown_sends_close_notify() {
    use ntex::{codec::BytesCodec, connect::Connect, service::Pipeline};
    use std::io::{Read, Write};
    use tls_openssl::ssl::ShutdownState;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        let mut stream = ssl_acceptor().accept(sock).unwrap();
        stream.write_all(b"test").unwrap();
        let mut buf = [0u8; 64];
        let mut received = Vec::new();
        let result = loop {
            match stream.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(e) => break Err(e.to_string()),
            }
        };
        let close_notify = stream.get_shutdown().contains(ShutdownState::RECEIVED);
        tx.send((received, result, close_notify)).unwrap();
    });

    let conn = Pipeline::new(SharedCfg::default(), schannel_connector());
    let io = conn
        .call(Connect::new("localhost").set_addr(Some(addr)))
        .await
        .unwrap();
    assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), "test");
    io.encode(ntex::util::Bytes::from_static(b"bye"), &BytesCodec)
        .unwrap();
    io.shutdown().await.unwrap();
    drop(io);

    let (received, result, close_notify) = rx.recv().unwrap();
    server.join().unwrap();
    assert_eq!(received, b"bye");
    assert_eq!(result, Ok(()));
    assert!(close_notify, "peer did not receive close_notify");
}
