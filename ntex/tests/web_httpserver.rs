#![recursion_limit = "256"]
use std::{sync::mpsc, thread, time::Duration};

#[cfg(feature = "openssl")]
use tls_openssl::ssl::SslAcceptorBuilder;

#[cfg(feature = "rustls")]
mod rustls_utils;

use ntex::http::HttpServiceConfig;
use ntex::web::{self, App, HttpResponse, HttpServer, WebAppConfig};
use ntex::{SharedCfg, io::IoConfig, server::TestServer, time::Seconds};
#[cfg(unix)]
use ntex::{rt, time::sleep};
use ntex_tls::TlsConfig;

#[ntex::test]
async fn test_run() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);

        sys.run(move || {
            let srv = HttpServer::new(async |_| {
                App::new().service(
                    web::resource("/").route(web::to(async || HttpResponse::Ok().body("test"))),
                )
            })
            .workers(1)
            .backlog(1)
            .max_connections(10)
            .max_tls_handshakes(10)
            .server_hostname("localhost")
            .stop_runtime()
            .disable_signals()
            .bind(
                format!("{addr}"),
                ntex::SharedCfg::new("WEB")
                    .add(
                        HttpServiceConfig::new()
                            .set_keepalive(10)
                            .set_client_timeout(Seconds(5)),
                    )
                    .add(IoConfig::new().set_shutdown_timeout(Seconds(1)))
                    .add(TlsConfig::new().set_handshake_timeout(Seconds(1))),
            )
            .unwrap()
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    use ntex::client;

    let client = client::Client::with_config(
        ntex::SharedCfg::new("DBG").add(IoConfig::new().set_connect_timeout(30)),
    );

    let host = format!("http://{addr}");
    let response = client.get(host.clone()).send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    srv.stop(false).await;

    thread::sleep(Duration::from_millis(25));
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
        .openssl(builder.build())
        .build(
            SharedCfg::new("TEST")
                .add(IoConfig::new().set_connect_timeout(30))
                .add(ntex::client::ClientConfig::new().set_response_timeout(Seconds(30))),
        )
}

#[ntex::test]
#[cfg(feature = "openssl")]
async fn test_openssl() {
    use ntex::web::HttpRequest;

    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        let builder = ssl_acceptor().unwrap();

        sys.run(move || {
            let srv = HttpServer::new(async |_| {
                App::new().service(web::resource("/").route(web::to(
                    async move |req: HttpRequest| {
                        assert!(req.app_config().secure());
                        HttpResponse::Ok().body("test")
                    },
                )))
            })
            .workers(1)
            .graceful_shutdown_timeout(Seconds(1))
            .stop_runtime()
            .disable_signals()
            .bind_openssl(
                format!("{addr}"),
                builder,
                SharedCfg::new("WEB").add(WebAppConfig::new().set_secure()),
            )
            .unwrap()
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
    srv.stop(false).await;

    thread::sleep(Duration::from_millis(25));
    sys.stop();
}

#[ntex::test]
#[cfg(all(unix, feature = "rustls", feature = "openssl"))]
async fn test_rustls() {
    use ntex::web::HttpRequest;

    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        let config = rustls_utils::tls_acceptor();

        sys.run(move || {
            let srv = HttpServer::new(async |_| {
                App::new().service(web::resource("/").route(web::to(async |req: HttpRequest| {
                    assert!(req.app_config().secure());
                    HttpResponse::Ok().body("test")
                })))
            })
            .workers(1)
            .graceful_shutdown_timeout(Seconds(1))
            .stop_runtime()
            .disable_signals()
            .bind_rustls(
                format!("{addr}"),
                &config,
                SharedCfg::new("WEB").add(WebAppConfig::new().set_secure()),
            )
            .unwrap()
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
    srv.stop(false).await;

    sleep(Duration::from_millis(25)).await;
    sys.stop();
}

#[ntex::test]
#[cfg(unix)]
async fn test_bind_uds() {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);

        sys.run(move || {
            let srv = HttpServer::new(async |_| {
                App::new().service(
                    web::resource("/").route(web::to(async || HttpResponse::Ok().body("test"))),
                )
            })
            .workers(1)
            .graceful_shutdown_timeout(Seconds(1))
            .stop_runtime()
            .disable_signals()
            .bind_uds("/tmp/uds-test", SharedCfg::default())
            .unwrap()
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    use ntex::client;

    let client = client::Client::builder()
        .connector(async |st: &SharedCfg, _| {
            Ok(rt::unix_connect("/tmp/uds-test", st.clone())
                .await
                .map_err(ntex::connect::ConnectError::from)?)
        })
        .build(SharedCfg::default());
    let response = client.get("http://localhost").send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    srv.stop(false).await;

    sleep(Duration::from_millis(25)).await;
    sys.stop();
}

#[ntex::test]
#[cfg(unix)]
async fn test_listen_uds() {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);

        sys.run(move || {
            let _ = std::fs::remove_file("/tmp/uds-test2");
            let lst = std::os::unix::net::UnixListener::bind("/tmp/uds-test2").unwrap();

            let srv = HttpServer::new(async |_| {
                App::new().service(
                    web::resource("/").route(web::to(async || HttpResponse::Ok().body("test"))),
                )
            })
            .workers(1)
            .graceful_shutdown_timeout(Seconds(1))
            .stop_runtime()
            .disable_signals()
            .listen_uds(lst, SharedCfg::default())
            .unwrap()
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    use ntex::client;

    let client = client::Client::builder()
        .connector(async |st: &SharedCfg, _| {
            Ok(rt::unix_connect("/tmp/uds-test2", st.clone())
                .await
                .map_err(ntex::connect::ConnectError::from)?)
        })
        .build(SharedCfg::default());
    let response = client.get("http://localhost").send().await.unwrap();
    assert!(response.status().is_success());

    // stop
    srv.stop(false).await;

    sleep(Duration::from_millis(25)).await;
    sys.stop();
}

#[derive(Clone)]
struct Counter(usize);

impl web::State for Counter {
    type Error = web::DefaultError;
}

struct CounterConfig;

impl ntex::server::ServerAppConfig for CounterConfig {
    type State = Counter;

    async fn create(&self) -> std::io::Result<Counter> {
        Ok(Counter(42))
    }
}

#[ntex::test]
async fn test_with_config() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);

        sys.run(move || {
            let srv = web::server_with_config(CounterConfig, async |st: &Counter| {
                assert_eq!(st.0, 42);
                App::new().service(web::resource("/").route(web::get().to_with_state(
                    async |st: &Counter, _: ()| HttpResponse::Ok().body(format!("{}", st.0)),
                )))
            })
            .workers(1)
            .stop_on_panic()
            .enable_affinity()
            .graceful_shutdown()
            .stop_runtime()
            .disable_signals()
            .bind(format!("{addr}"), SharedCfg::default())
            .unwrap()
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    let client = ntex::client::Client::new();
    let response = client.get(format!("http://{addr}")).send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.body().await.unwrap(), "42");

    srv.stop(false).await;
    thread::sleep(Duration::from_millis(25));
    sys.stop();
}

#[ntex::test]
async fn test_bind_error() {
    let lst = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();

    let res = HttpServer::new(async |_| {
        App::new().service(web::resource("/").to(async || HttpResponse::Ok()))
    })
    .disable_signals()
    .bind(addr, SharedCfg::default());
    assert_eq!(
        res.err().unwrap().kind(),
        std::io::ErrorKind::AddrInUse,
        "address is in use"
    );

    let res = HttpServer::new(async |_| {
        App::new().service(web::resource("/").to(async || HttpResponse::Ok()))
    })
    .disable_signals()
    .bind(&[][..] as &[std::net::SocketAddr], SharedCfg::default());
    assert_eq!(
        res.err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput,
        "no addresses"
    );
}

#[cfg(feature = "openssl")]
async fn client_alpn(protos: &[u8]) -> ntex::client::Client {
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    builder.set_alpn_protos(protos).unwrap();

    ntex::client::Client::builder()
        .openssl(builder.build())
        .build(SharedCfg::default())
}

#[ntex::test]
#[cfg(feature = "openssl")]
async fn test_listen_openssl() {
    let lst = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        let builder = ssl_acceptor().unwrap();

        sys.run(move || {
            let srv = HttpServer::new(async |_| {
                App::new().service(web::resource("/").route(web::to(
                    async |req: web::HttpRequest| {
                        HttpResponse::Ok().body(format!("{:?}", req.version()))
                    },
                )))
            })
            .workers(1)
            .stop_runtime()
            .disable_signals()
            .listen_openssl(lst, SharedCfg::default(), builder)
            .unwrap()
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    let host = format!("https://{addr}");
    for (protos, version) in [
        (&b"\x02h2\x08http/1.1"[..], "HTTP/2.0"),
        (&b"\x08http/1.1"[..], "HTTP/1.1"),
        // unknown protocol, server does not select alpn protocol
        (&b"\x06spdy/1"[..], "HTTP/1.1"),
    ] {
        let client = client_alpn(protos).await;
        let response = client.get(host.clone()).send().await.unwrap();
        assert!(response.status().is_success());
        assert_eq!(response.body().await.unwrap(), version);
    }

    srv.stop(false).await;
    thread::sleep(Duration::from_millis(25));
    sys.stop();
}

#[ntex::test]
#[cfg(all(feature = "rustls", feature = "openssl"))]
async fn test_listen_rustls() {
    let lst = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        let config = rustls_utils::tls_acceptor();

        sys.run(move || {
            let srv = HttpServer::new(async |_| {
                App::new().service(
                    web::resource("/").route(web::to(async || HttpResponse::Ok().body("test"))),
                )
            })
            .workers(1)
            .stop_runtime()
            .disable_signals()
            .listen_rustls(lst, SharedCfg::default(), config)
            .unwrap()
            .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    let client = client().await;
    let response = client
        .get(format!("https://localhost:{}", addr.port()))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    srv.stop(false).await;
    thread::sleep(Duration::from_millis(25));
    sys.stop();
}
