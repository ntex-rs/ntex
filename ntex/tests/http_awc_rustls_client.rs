#![cfg(feature = "rustls")]
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tls_openssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslVerifyMode};

use ntex::client::{Client, Connector};
use ntex::http::{HttpService, test::server as test_server};
use ntex::service::{ServiceFactory, cfg::SharedCfg, chain_factory};
use ntex::util::Ready;
use ntex::web::{self, App, HttpResponse};

mod rustls_utils;

fn ssl_acceptor() -> SslAcceptor {
    // load ssl keys
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder.set_verify_callback(SslVerifyMode::NONE, |_, _| true);
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
            Err(tls_openssl::ssl::AlpnError::NOACK)
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
            .openssl(ssl_acceptor())
            .map_err(|_| ()),
        )
    })
    .await;

    // disable ssl verification
    let mut config = rustls_utils::tls_connector();
    let protos = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config.alpn_protocols = protos;

    let client = Client::builder()
        .connector::<&str>(Connector::default().rustls(config))
        .build(SharedCfg::default())
        .await
        .unwrap();

    // req 1
    let _response = client.get(srv.surl("/")).send().await;
    // assert!(response.status().is_success());

    // req 2
    let _response = client.post(srv.surl("/")).send().await;
    //assert!(response.status().is_success());
    //assert_eq!(response.version(), Version::HTTP_2);

    // one connection
    //assert_eq!(num.load(Ordering::Relaxed), 1);
}
