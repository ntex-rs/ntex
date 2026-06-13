#![cfg(all(windows, feature = "openssl"))]

use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};

use ntex::client::{Client, Connector};
use ntex::http::{HttpService, Uri, Version, test::server as test_server};
use ntex::service::{cfg::SharedCfg, chain_factory};
use ntex::util::Ready;
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

    let srv = test_server(async move || {
        let num2 = num2.clone();
        chain_factory(move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ready::Ok(io)
        })
        .and_then(
            HttpService::h2(App::new().service(
                web::resource("/").route(web::to(|| async { HttpResponse::Ok() })),
            ))
            .openssl(ssl_acceptor()),
        )
    })
    .await;

    let tls = TlsConnector::<ntex::connect::Connector<Uri>>::with_config(
        ClientConfig::new().danger_accept_invalid_certs(true),
    );
    let client = Client::builder()
        .connector::<&str>(Connector::default().secure_connector(tls))
        .build(SharedCfg::default())
        .await
        .unwrap();

    let response = client.get(srv.surl("/")).send().await.unwrap();
    assert!(response.status().is_success());

    let response = client.post(srv.surl("/")).send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.version(), Version::HTTP_2);

    assert_eq!(num.load(Ordering::Relaxed), 1);
}
