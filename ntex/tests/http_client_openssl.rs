#![recursion_limit = "256"]
#![cfg(feature = "openssl")]
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};

use tls_openssl::ssl::{
    AlpnError, SslAcceptor, SslConnector, SslFiletype, SslMethod, SslVerifyMode,
};

use ntex::client::Client;
use ntex::http::{self, HttpService, Version, test::server as test_server};
use ntex::web::{self, App, HttpResponse};
use ntex::{SharedCfg, service, time::Seconds};

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

    let srv = test_server(async move |_| {
        let num2 = num2.clone();
        service(async move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ok(io)
        })
        .and_then(http::openssl(
            ssl_acceptor(),
            HttpService::h2(
                App::new().service(web::resource("/").route(web::to(async || HttpResponse::Ok()))),
            ),
        ))
    });

    // disable ssl verification
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let _ = builder
        .set_alpn_protos(b"\x02h2\x08http/1.1")
        .map_err(|e| log::error!("Cannot set alpn protocol: {e:?}"));

    let client = Client::builder()
        .openssl(builder.build())
        .build(SharedCfg::default());

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

#[ntex::test]
async fn test_h2_stream_limit_waits_for_payload() {
    use ntex::client::ClientConfig;
    use ntex::http::{Request, Response};
    use ntex::time::{Millis, now, sleep};
    use ntex::util::Bytes;

    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(async move |_| {
        let num2 = num2.clone();
        service(async move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ok(io)
        })
        .and_then(http::openssl(
            ssl_acceptor(),
            HttpService::h2(async |req: Request| {
                let slow = req.path() == "/slow";
                let body = futures_util::stream::once(Box::pin(async move {
                    if slow {
                        sleep(Millis(500)).await;
                    }
                    Ok::<_, std::io::Error>(Bytes::from_static(b"data"))
                }));
                Ok::<_, std::io::Error>(Response::Ok().streaming(body))
            }),
        ))
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let _ = builder.set_alpn_protos(b"\x02h2\x08http/1.1");

    let client = Client::builder().openssl(builder.build()).build(
        SharedCfg::new("CLI")
            .add(
                ClientConfig::new()
                    .set_h2_connection_limit(1)
                    .set_h2_max_streams(1),
            )
            .build(),
    );

    // response head is received, payload is not completed yet
    let response = client.get(srv.surl("/slow")).send().await.unwrap();
    assert_eq!(response.version(), Version::HTTP_2);

    // second request waits for the free stream
    let start = now();
    let response2 = client.get(srv.surl("/")).send().await.unwrap();
    assert!(now() - start >= std::time::Duration::from_millis(300));
    assert!(response2.status().is_success());

    assert_eq!(response.body().await.unwrap(), Bytes::from_static(b"data"));
    assert_eq!(num.load(Ordering::Relaxed), 1);
}
