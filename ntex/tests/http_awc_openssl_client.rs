#![cfg(feature = "openssl")]
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};

use tls_openssl::ssl::{
    AlpnError, SslAcceptor, SslConnector, SslFiletype, SslMethod, SslVerifyMode,
};

use ntex::http::client::{Client, Connector};
use ntex::http::test::server as test_server;
use ntex::http::{HttpService, Version};
use ntex::service::{chain_factory, map_config};
use ntex::web::{self, App, HttpResponse, dev::AppConfig};
use ntex::{io::SharedConfig, time::Seconds, util::Ready};

fn ssl_acceptor() -> SslAcceptor {
    // load ssl keys
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

    let srv = test_server(move || {
        let num2 = num2.clone();
        chain_factory(move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ready::Ok(io)
        })
        .and_then(
            HttpService::h2(map_config(
                App::new().service(
                    web::resource("/").route(web::to(|| async { HttpResponse::Ok() })),
                ),
                |_| AppConfig::default(),
            ))
            .openssl(ssl_acceptor()), //.map_err(|_| ()),
        )
    })
    .await;

    // disable ssl verification
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let _ = builder
        .set_alpn_protos(b"\x02h2\x08http/1.1")
        .map_err(|e| log::error!("Cannot set alpn protocol: {e:?}"));

    let client = Client::build()
        .connector::<&str>(Connector::default().openssl(builder.build()))
        .finish(SharedConfig::default())
        .await
        .unwrap();

    // req 1
    let request = client.get(srv.surl("/")).timeout(Seconds(30)).send();
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // req 2
    let req = client.post(srv.surl("/"));
    let response = req.send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.version(), Version::HTTP_2);

    // one connection
    assert_eq!(num.load(Ordering::Relaxed), 1);
}
