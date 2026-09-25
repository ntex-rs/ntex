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
