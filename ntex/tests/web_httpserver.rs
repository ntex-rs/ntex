use std::sync::mpsc;
use std::{thread, time::Duration};

#[cfg(feature = "openssl")]
use open_ssl::ssl::SslAcceptorBuilder;

use ntex::server::TestServer;
use ntex::web::{self, App, HttpResponse, HttpServer};

#[cfg(unix)]
#[ntex::test]
async fn test_run() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let mut sys = ntex::rt::System::new("test");

        let srv = sys.exec(|| {
            HttpServer::new(|| {
                App::new().service(
                    web::resource("/")
                        .route(web::to(|| async { HttpResponse::Ok().body("test") })),
                )
            })
            .workers(1)
            .backlog(1)
            .maxconn(10)
            .maxconnrate(10)
            .keep_alive(10)
            .client_timeout(5000)
            .disconnect_timeout(1000)
            .ssl_handshake_timeout(1000)
            .server_hostname("localhost")
            .stop_runtime()
            .disable_signals()
            .bind(format!("{}", addr))
            .unwrap()
            .run()
        });

        let _ = tx.send((srv, ntex::rt::System::current()));
        let _ = sys.run();
    });
    let (srv, sys) = rx.recv().unwrap();

    use ntex::http::client;

    let client = client::Client::build()
        .connector(
            client::Connector::default()
                .timeout(Duration::from_millis(100))
                .finish(),
        )
        .finish();

    let host = format!("http://{}", addr);
    let response = client.get(host.clone()).send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    thread::sleep(Duration::from_millis(100));
    sys.stop();
}

#[cfg(feature = "openssl")]
fn ssl_acceptor() -> std::io::Result<SslAcceptorBuilder> {
    use open_ssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslVerifyMode};
    // load ssl keys
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    builder
        .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./tests/cert.pem")
        .unwrap();
    Ok(builder)
}

#[cfg(feature = "openssl")]
fn client() -> ntex::http::client::Client {
    use open_ssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let _ = builder
        .set_alpn_protos(b"\x02h2\x08http/1.1")
        .map_err(|e| log::error!("Cannot set alpn protocol: {:?}", e));

    ntex::http::client::Client::build()
        .timeout(Duration::from_millis(30000))
        .connector(
            ntex::http::client::Connector::default()
                .timeout(Duration::from_millis(30000))
                .openssl(builder.build())
                .finish(),
        )
        .finish()
}

#[ntex::test]
#[cfg(feature = "openssl")]
async fn test_openssl() {
    use ntex::web::HttpRequest;

    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let mut sys = ntex::rt::System::new("test");
        let builder = ssl_acceptor().unwrap();

        let srv = sys.exec(|| {
            HttpServer::new(|| {
                App::new().service(web::resource("/").route(web::to(
                    |req: HttpRequest| async move {
                        assert!(req.app_config().secure());
                        HttpResponse::Ok().body("test")
                    },
                )))
            })
            .workers(1)
            .shutdown_timeout(1)
            .stop_runtime()
            .disable_signals()
            .bind_openssl(format!("{}", addr), builder)
            .unwrap()
            .run()
        });

        let _ = tx.send((srv, ntex::rt::System::current()));
        let _ = sys.run();
    });
    let (srv, sys) = rx.recv().unwrap();

    let client = client();
    let host = format!("https://{}", addr);
    let response = client.get(host.clone()).send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    thread::sleep(Duration::from_millis(100));
    sys.stop();
}

#[ntex::test]
#[cfg(all(feature = "rustls", feature = "openssl"))]
async fn test_rustls() {
    use std::fs::File;
    use std::io::BufReader;

    use ntex::web::HttpRequest;
    use rust_tls::{
        internal::pemfile::{certs, pkcs8_private_keys},
        NoClientAuth, ServerConfig as RustlsServerConfig,
    };

    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let mut sys = ntex::rt::System::new("test");

        // load ssl keys
        let mut config = RustlsServerConfig::new(NoClientAuth::new());
        let cert_file = &mut BufReader::new(File::open("./tests/cert.pem").unwrap());
        let key_file = &mut BufReader::new(File::open("./tests/key.pem").unwrap());
        let cert_chain = certs(cert_file).unwrap();
        let mut keys = pkcs8_private_keys(key_file).unwrap();
        config.set_single_cert(cert_chain, keys.remove(0)).unwrap();

        let srv = sys.exec(|| {
            HttpServer::new(|| {
                App::new().service(web::resource("/").route(web::to(
                    |req: HttpRequest| async move {
                        assert!(req.app_config().secure());
                        HttpResponse::Ok().body("test")
                    },
                )))
            })
            .workers(1)
            .shutdown_timeout(1)
            .stop_runtime()
            .disable_signals()
            .bind_rustls(format!("{}", addr), config)
            .unwrap()
            .run()
        });

        let _ = tx.send((srv, ntex::rt::System::current()));
        let _ = sys.run();
    });
    let (srv, sys) = rx.recv().unwrap();

    let client = client();
    let host = format!("https://localhost:{}", addr.port());
    let response = client.get(host).send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    thread::sleep(Duration::from_millis(100));
    sys.stop();
}

#[ntex::test]
#[cfg(unix)]
async fn test_bind_uds() {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let mut sys = ntex::rt::System::new("test");

        let srv = sys.exec(|| {
            HttpServer::new(|| {
                App::new().service(
                    web::resource("/")
                        .route(web::to(|| async { HttpResponse::Ok().body("test") })),
                )
            })
            .workers(1)
            .shutdown_timeout(1)
            .stop_runtime()
            .disable_signals()
            .bind_uds("/tmp/uds-test")
            .unwrap()
            .run()
        });

        let _ = tx.send((srv, ntex::rt::System::current()));
        let _ = sys.run();
    });
    let (srv, sys) = rx.recv().unwrap();

    use ntex::http::client;

    let client = client::Client::build()
        .connector(
            client::Connector::default()
                .connector(ntex::fn_service(|_| async {
                    let stream =
                        ntex::rt::net::UnixStream::connect("/tmp/uds-test").await?;
                    Ok((stream, ntex::http::Protocol::Http1))
                }))
                .finish(),
        )
        .finish();
    let response = client.get("http://localhost").send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    thread::sleep(Duration::from_millis(100));
    sys.stop();
}

#[ntex::test]
#[cfg(unix)]
async fn test_listen_uds() {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let mut sys = ntex::rt::System::new("test");

        let srv = sys.exec(|| {
            let lst = std::os::unix::net::UnixListener::bind("/tmp/uds-test2").unwrap();

            HttpServer::new(|| {
                App::new().service(
                    web::resource("/")
                        .route(web::to(|| async { HttpResponse::Ok().body("test") })),
                )
            })
            .workers(1)
            .shutdown_timeout(1)
            .stop_runtime()
            .disable_signals()
            .listen_uds(lst)
            .unwrap()
            .run()
        });

        let _ = tx.send((srv, ntex::rt::System::current()));
        let _ = sys.run();
    });
    let (srv, sys) = rx.recv().unwrap();

    use ntex::http::client;

    let client = client::Client::build()
        .connector(
            client::Connector::default()
                .connector(ntex::fn_service(|_| async {
                    let stream =
                        ntex::rt::net::UnixStream::connect("/tmp/uds-test2").await?;
                    Ok((stream, ntex::http::Protocol::Http1))
                }))
                .finish(),
        )
        .finish();
    let response = client.get("http://localhost").send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    thread::sleep(Duration::from_millis(100));
    sys.stop();
}
