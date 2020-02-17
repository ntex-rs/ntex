//! Test server
use std::sync::mpsc;
use std::{net, thread, time};

use actix_rt::{net::TcpStream, System};
use net2::TcpBuilder;

use super::{Server, ServiceFactory};
use crate::http::client::{Client, Connector};

/// Start test server
///
/// `TestServer` is very simple test server that simplify process of writing
/// integration tests cases for actix web applications.
///
/// # Examples
///
/// ```rust
/// use ntex::http;
/// use ntex::server;
/// use ntex::web::{self, App, HttpResponse};
///
/// async fn my_handler() -> Result<HttpResponse, http::Error> {
///     Ok(HttpResponse::Ok().into())
/// }
///
/// #[ntex::test]
/// async fn test_example() {
///     let mut srv = server::test_server(
///         || http::HttpService::new(
///             App::new().service(
///                 web::resource("/").to(my_handler))
///         )
///     );
///
///     let req = srv.get("/");
///     let response = req.send().await.unwrap();
///     assert!(response.status().is_success());
/// }
/// ```
pub fn test_server<F: ServiceFactory<TcpStream>>(factory: F) -> TestServer {
    let (tx, rx) = mpsc::channel();

    // run server in separate thread
    thread::spawn(move || {
        let sys = System::new("ntex-test-server");
        let tcp = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local_addr = tcp.local_addr().unwrap();

        Server::build()
            .listen("test", tcp, factory)?
            .workers(1)
            .disable_signals()
            .start();

        tx.send((System::current(), local_addr)).unwrap();
        sys.run()
    });

    let (system, addr) = rx.recv().unwrap();

    let client = {
        let connector = {
            #[cfg(feature = "openssl")]
            {
                use open_ssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

                let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
                builder.set_verify(SslVerifyMode::NONE);
                let _ = builder
                    .set_alpn_protos(b"\x02h2\x08http/1.1")
                    .map_err(|e| log::error!("Can not set alpn protocol: {:?}", e));
                Connector::new()
                    .conn_lifetime(time::Duration::from_secs(0))
                    .timeout(time::Duration::from_millis(30000))
                    .ssl(builder.build())
                    .finish()
            }
            #[cfg(not(feature = "openssl"))]
            {
                Connector::new()
                    .conn_lifetime(time::Duration::from_secs(0))
                    .timeout(time::Duration::from_millis(30000))
                    .finish()
            }
        };

        Client::build().connector(connector).finish()
    };
    actix_connect::start_default_resolver();

    TestServer {
        addr,
        client,
        system,
        host: "127.0.0.1".to_string(),
        port: addr.port(),
    }
}

/// Test server controller
pub struct TestServer {
    addr: net::SocketAddr,
    host: String,
    port: u16,
    client: Client,
    system: System,
}

impl TestServer {
    /// Test server host
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Test server port
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Test server socket addr
    pub fn addr(&self) -> net::SocketAddr {
        self.addr
    }

    /// Construct test server url
    pub fn url(&self, uri: &str) -> String {
        if uri.starts_with('/') {
            format!("http://localhost:{}{}", self.addr.port(), uri)
        } else {
            format!("http://localhost:{}/{}", self.addr.port(), uri)
        }
    }

    /// Construct test https server url
    pub fn surl(&self, uri: &str) -> String {
        if uri.starts_with('/') {
            format!("https://localhost:{}{}", self.addr.port(), uri)
        } else {
            format!("https://localhost:{}/{}", self.addr.port(), uri)
        }
    }

    /// Returns http client reference
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Connect to server, return TcpStream
    pub fn connect(&self) -> std::io::Result<TcpStream> {
        TcpStream::from_std(net::TcpStream::connect(self.addr)?)
    }

    /// Stop http server
    fn stop(&mut self) {
        self.system.stop();
    }

    /// Get first available unused address
    pub fn unused_addr() -> net::SocketAddr {
        let addr: net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = TcpBuilder::new_v4().unwrap();
        socket.bind(&addr).unwrap();
        socket.reuse_address(true).unwrap();
        let tcp = socket.to_tcp_listener().unwrap();
        tcp.local_addr().unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop()
    }
}
