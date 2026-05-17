#![cfg(feature = "rustls")]
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};

use ntex::client::{Client, Connector};
use ntex::http::{HttpService, Version, test::server as test_server};
use ntex::service::{ServiceFactory, cfg::SharedCfg, chain_factory};
use ntex::util::Ready;
use ntex::web::{self, App, HttpResponse};

mod rustls_utils;

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
            .rustls(rustls_utils::tls_acceptor())
            .map_err(|_| ()),
        )
    })
    .await;

    // disable ssl verification
    let mut config = rustls_utils::tls_connector();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let client = Client::builder()
        .connector::<&str>(Connector::default().rustls(config))
        .build(SharedCfg::default())
        .await
        .unwrap();

    // req 1
    let response = client.get(srv.surl("/")).send().await.unwrap();
    assert!(response.status().is_success());

    // req 2
    let response = client.post(srv.surl("/")).send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.version(), Version::HTTP_2);

    // one connection
    assert_eq!(num.load(Ordering::Relaxed), 1);
}
