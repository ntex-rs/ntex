#![cfg(feature = "rustls")]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::future::ok;
use open_ssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslVerifyMode};
use rust_tls::ClientConfig;

use ntex::http::client::{Client, Connector};
use ntex::http::test::server as test_server;
use ntex::http::HttpService;
use ntex::service::{map_config, pipeline_factory, ServiceFactory};
use ntex::web::{self, dev::AppConfig, App, HttpResponse};

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
            Err(open_ssl::ssl::AlpnError::NOACK)
        }
    });
    builder.set_alpn_protos(b"\x02h2").unwrap();
    builder.build()
}

mod danger {
    pub struct NoCertificateVerification {}

    impl rust_tls::ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _roots: &rust_tls::RootCertStore,
            _presented_certs: &[rust_tls::Certificate],
            _dns_name: webpki::DNSNameRef<'_>,
            _ocsp: &[u8],
        ) -> Result<rust_tls::ServerCertVerified, rust_tls::TLSError> {
            Ok(rust_tls::ServerCertVerified::assertion())
        }
    }
}

#[ntex::test]
async fn test_connection_reuse_h2() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(move || {
        let num2 = num2.clone();
        pipeline_factory(move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            ok(io)
        })
        .and_then(
            HttpService::build()
                .h2(map_config(
                    App::new().service(
                        web::resource("/")
                            .route(web::to(|| async { HttpResponse::Ok() })),
                    ),
                    |_| AppConfig::default(),
                ))
                .openssl(ssl_acceptor())
                .map_err(|_| ()),
        )
    });

    // disable ssl verification
    let mut config = ClientConfig::new();
    let protos = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config.set_protocols(&protos);
    config
        .dangerous()
        .set_certificate_verifier(Arc::new(danger::NoCertificateVerification {}));

    let client = Client::build()
        .connector(Connector::new().rustls(Arc::new(config)).finish())
        .finish();

    // req 1
    let _response = client.get(srv.surl("/")).send().await;

    // let response = request.await.unwrap();
    // assert!(response.status().is_success());

    // req 2
    let _response = client.post(srv.surl("/")).send().await;
    // let response = req.send().await.unwrap();
    //assert!(response.status().is_success());
    //assert_eq!(response.version(), Version::HTTP_2);

    // one connection
    // assert_eq!(num.load(Ordering::Relaxed), 1);
}
