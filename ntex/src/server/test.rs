//! Test server
use std::sync::mpsc;
use std::{net, thread};

use actix_rt::{net::TcpStream, System};
use net2::TcpBuilder;

use super::{Server, ServiceFactory};

/// Start test server
///
/// `TestServer` is very simple test server that simplify process of writing
/// integration tests cases for actix web applications.
///
/// # Examples
///
/// ```rust
/// use ntex::http;
/// use ntex::http::client::Client;
/// use ntex::server;
/// use ntex::web::{self, App, HttpResponse};
///
/// async fn my_handler() -> Result<HttpResponse, std::io::Error> {
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
///     let req = Client::new().get("http://127.0.0.1:{}", srv.addr().port());
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

    TestServer { addr, system }
}

/// Test server controller
pub struct TestServer {
    addr: net::SocketAddr,
    system: System,
}

impl TestServer {
    /// Test server socket addr
    pub fn addr(&self) -> net::SocketAddr {
        self.addr
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
