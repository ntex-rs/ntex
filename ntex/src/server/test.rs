//! Test server
use std::{io, net, sync::mpsc, thread};

use socket2::{Domain, SockAddr, Socket, Type};

use crate::rt::{net::TcpStream, System};
use crate::server::{Server, ServerBuilder, StreamServiceFactory};

/// Start test server
///
/// `TestServer` is very simple test server that simplify process of writing
/// integration tests cases for ntex web applications.
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
pub fn test_server<F: StreamServiceFactory<TcpStream>>(factory: F) -> TestServer {
    let (tx, rx) = mpsc::channel();

    // run server in separate thread
    thread::spawn(move || {
        let mut sys = System::new("ntex-test-server");
        let tcp = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local_addr = tcp.local_addr().unwrap();

        sys.exec(|| {
            Server::build()
                .listen("test", tcp, factory)?
                .workers(1)
                .disable_signals()
                .start();
            Ok::<_, io::Error>(())
        })?;

        tx.send((System::current(), local_addr)).unwrap();
        sys.run()
    });

    let (system, addr) = rx.recv().unwrap();

    TestServer { addr, system }
}

/// Start new server with server builder
pub fn build_test_server<F>(factory: F) -> TestServer
where
    F: FnOnce(ServerBuilder) -> ServerBuilder + Send + 'static,
{
    let (tx, rx) = mpsc::channel();

    // run server in separate thread
    thread::spawn(move || {
        let mut sys = System::new("ntex-test-server");

        sys.exec(|| {
            factory(Server::build())
                .workers(1)
                .disable_signals()
                .start();
        });

        tx.send(System::current()).unwrap();
        sys.run()
    });
    let system = rx.recv().unwrap();

    TestServer {
        system,
        addr: "127.0.0.1:0".parse().unwrap(),
    }
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
    pub async fn connect(&self) -> io::Result<TcpStream> {
        TcpStream::connect(self.addr).await
    }

    /// Stop http server
    fn stop(&mut self) {
        self.system.stop();
    }

    /// Get first available unused address
    pub fn unused_addr() -> net::SocketAddr {
        let addr: net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket.set_reuse_address(true).unwrap();
        socket.bind(&SockAddr::from(addr)).unwrap();
        let tcp = net::TcpListener::from(socket);
        tcp.local_addr().unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop()
    }
}
