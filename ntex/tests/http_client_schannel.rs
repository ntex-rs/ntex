#![recursion_limit = "256"]
#![cfg(all(windows, feature = "schannel"))]

use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};

use ntex::client::Client;
use ntex::http::{HttpService, Uri, Version, openssl, test::server as test_server};
use ntex::service::{cfg::SharedCfg, service};
use ntex::web::{self, App, HttpResponse};
use ntex_tls::schannel::{ClientConfig, ServerConfig, TlsConnector};

fn server_config() -> ServerConfig {
    ServerConfig::from_pem(include_str!("cert.pem"), include_str!("key.pem")).unwrap()
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
        .and_then(schannel(
            server_config(),
            HttpService::h2(
                App::new().service(web::resource("/").route(web::to(async || HttpResponse::Ok()))),
            ),
        ))
    });

    let tls = TlsConnector::<ntex::connect::Connector<ntex::http::Uri>>::with_config(
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

#[ntex::test]
async fn test_schannel_public_https() {
    let tls = TlsConnector::<ntex::connect::Connector<ntex::http::Uri>>::new();
    let client = Client::builder()
        .secure_connector(tls)
        .build(SharedCfg::default());

    let response = client.get("https://example.com/").send().await.unwrap();
    assert!(response.status().is_success());
    let body = response.body().await.unwrap();
    assert!(!body.is_empty());
}

#[ntex::test]
async fn test_schannel_bing_https() {
    let tls = TlsConnector::<ntex::connect::Connector<ntex::http::Uri>>::new();
    let client = Client::builder().secure_connector(tls).build(
        ntex::client::ClientConfig::new()
            .disable_timeout()
            .set_response_payload_limit(usize::MAX),
    );

    let response = client
        .get("https://cn.bing.com/HPImageArchive.aspx?format=js&idx=0&n=1&mkt=zh-CN")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body = response.body().await.unwrap();
    assert!(!body.is_empty());
}
