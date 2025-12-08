#![allow(clippy::let_underscore_future)]
use std::{sync::mpsc, thread, time::Duration};

#[cfg(feature = "openssl")]
use tls_openssl::ssl::SslAcceptorBuilder;

#[cfg(feature = "rustls")]
mod rustls_utils;

use ntex::http::HttpServiceConfig;
use ntex::web::{self, App, HttpResponse, HttpServer, WebAppConfig};
use ntex::{SharedCfg, io::IoConfig, rt, server::TestServer, time::Seconds, time::sleep};
use ntex_tls::TlsConfig;

#[ntex::test]
async fn test_run() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test");

        sys.run(move || {
            let srv = HttpServer::new(async || {
                App::new().service(
                    web::resource("/")
                        .route(web::to(|| async { HttpResponse::Ok().body("test") })),
                )
            })
            .workers(1)
            .backlog(1)
            .maxconn(10)
            .maxconnrate(10)
            .server_hostname("localhost")
            .stop_runtime()
            .disable_signals()
            .config(
                ntex::SharedCfg::new("WEB")
                    .add(
                        HttpServiceConfig::new()
                            .set_keepalive(10)
                            .set_client_timeout(Seconds(5)),
                    )
                    .add(IoConfig::new().set_disconnect_timeout(Seconds(1)))
                    .add(TlsConfig::new().set_handshake_timeout(Seconds(1))),
            )
            .bind(format!("{addr}"))
            .unwrap()
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    use ntex::client;

    let client = client::Client::builder()
        .connector::<&str>(client::Connector::default())
        .build(ntex::SharedCfg::new("DBG").add(IoConfig::new().set_connect_timeout(30)))
        .await
        .unwrap();

    let host = format!("http://{addr}");
    let response = client.get(host.clone()).send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    thread::sleep(Duration::from_millis(100));
    sys.stop();
}

#[cfg(feature = "openssl")]
fn ssl_acceptor() -> std::io::Result<SslAcceptorBuilder> {
    use tls_openssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslVerifyMode};
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
async fn client() -> ntex::client::Client {
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let _ = builder
        .set_alpn_protos(b"\x02h2\x08http/1.1")
        .map_err(|e| log::error!("Cannot set alpn protocol: {e:?}"));

    ntex::client::Client::builder()
        .response_timeout(Seconds(30))
        .connector::<&str>(ntex::client::Connector::default().openssl(builder.build()))
        .build(ntex::SharedCfg::new("TEST").add(IoConfig::new().set_connect_timeout(30)))
        .await
        .unwrap()
}

#[ntex::test]
#[cfg(feature = "openssl")]
async fn test_openssl() {
    use ntex::web::HttpRequest;

    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test");
        let builder = ssl_acceptor().unwrap();

        sys.run(move || {
            let srv = HttpServer::new(async || {
                App::new().service(web::resource("/").route(web::to(
                    |req: HttpRequest| async move {
                        assert!(req.app_config().secure());
                        HttpResponse::Ok().body("test")
                    },
                )))
            })
            .workers(1)
            .shutdown_timeout(Seconds(1))
            .stop_runtime()
            .disable_signals()
            .bind_openssl(format!("{addr}"), builder)
            .unwrap()
            .config(SharedCfg::new("WEB").add(WebAppConfig::new().set_secure()))
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();
    thread::sleep(Duration::from_millis(100));

    let client = client().await;
    let host = format!("https://{addr}");
    let response = client.get(host.clone()).send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    thread::sleep(Duration::from_millis(100));
    sys.stop();
}

#[ntex::test]
#[cfg(all(unix, feature = "rustls", feature = "openssl"))]
async fn test_rustls() {
    use ntex::web::HttpRequest;

    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test");
        let config = rustls_utils::tls_acceptor();

        sys.run(move || {
            let srv = HttpServer::new(async || {
                App::new().service(web::resource("/").route(web::to(
                    async |req: HttpRequest| {
                        assert!(req.app_config().secure());
                        HttpResponse::Ok().body("test")
                    },
                )))
            })
            .workers(1)
            .shutdown_timeout(Seconds(1))
            .stop_runtime()
            .disable_signals()
            .bind_rustls(format!("{addr}"), config)
            .unwrap()
            .config(SharedCfg::new("WEB").add(WebAppConfig::new().set_secure()))
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    let client = client().await;
    let host = format!("https://localhost:{}", addr.port());
    let response = client.get(host).send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    sleep(Duration::from_millis(100)).await;
    sys.stop();
}

#[ntex::test]
#[cfg(unix)]
async fn test_bind_uds() {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test");

        sys.run(move || {
            let srv = HttpServer::new(async || {
                App::new().service(
                    web::resource("/")
                        .route(web::to(|| async { HttpResponse::Ok().body("test") })),
                )
            })
            .workers(1)
            .shutdown_timeout(Seconds(1))
            .stop_runtime()
            .disable_signals()
            .bind_uds("/tmp/uds-test")
            .unwrap()
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    use ntex::{ServiceFactory, client};

    let client = client::Client::builder()
        .connector::<&str>(
            client::Connector::default().connector(
                ntex::service::fn_service(|_| async {
                    Ok(
                        rt::unix_connect("/tmp/uds-test", ntex::SharedCfg::default())
                            .await?,
                    )
                })
                .map_init_err(|_| unreachable!()),
            ),
        )
        .build(ntex::SharedCfg::default())
        .await
        .unwrap();
    let response = client.get("http://localhost").send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    sleep(Duration::from_millis(100)).await;
    sys.stop();
}

#[ntex::test]
#[cfg(unix)]
async fn test_listen_uds() {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test");

        sys.run(move || {
            let _ = std::fs::remove_file("/tmp/uds-test2");
            let lst = std::os::unix::net::UnixListener::bind("/tmp/uds-test2").unwrap();

            let srv = HttpServer::new(async || {
                App::new().service(
                    web::resource("/")
                        .route(web::to(|| async { HttpResponse::Ok().body("test") })),
                )
            })
            .workers(1)
            .shutdown_timeout(Seconds(1))
            .stop_runtime()
            .disable_signals()
            .listen_uds(lst)
            .unwrap()
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    use ntex::{ServiceFactory, client};

    let client = client::Client::builder()
        .connector::<&str>(
            client::Connector::default().connector(
                ntex::service::fn_service(|_| async {
                    Ok(
                        rt::unix_connect("/tmp/uds-test2", ntex::SharedCfg::default())
                            .await?,
                    )
                })
                .map_init_err(|_| unreachable!()),
            ),
        )
        .build(ntex::SharedCfg::default())
        .await
        .unwrap();
    let response = client.get("http://localhost").send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    let _ = srv.stop(false);

    sleep(Duration::from_millis(100)).await;
    sys.stop();
}
