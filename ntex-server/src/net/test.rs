//! Test server
use std::{io, net, thread};

use ntex_net::{tcp_connect, Io};
use ntex_rt::System;
use ntex_service::ServiceFactory;
use socket2::{Domain, SockAddr, Socket, Type};

use super::{Server, ServerBuilder};

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
pub fn test_server<F, R>(factory: F) -> TestServer
where
    F: Fn() -> R + Send + Clone + 'static,
    R: ServiceFactory<Io> + 'static,
{
    let (tx, rx) = oneshot::channel();
    // run server in separate thread
    thread::spawn(move || {
        let sys = System::new("ntex-test-server");
        let tcp = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local_addr = tcp.local_addr().unwrap();
        let system = sys.system();

        sys.run(move || {
            let server = ServerBuilder::new()
                .listen("test", tcp, move |_| factory())?
                .set_tag("test", "TEST-SERVER")
                .workers(1)
                .disable_signals()
                .run();

            ntex_rt::spawn(async move {
                ntex_util::time::sleep(ntex_util::time::Millis(75)).await;
                tx.send((system, local_addr, server))
                    .expect("Failed to send Server to TestServer");
            });

            Ok(())
        })
    });

    let (system, addr, server) = rx.recv().unwrap();

    TestServer {
        addr,
        server,
        system,
    }
}

/// Start new server with server builder
pub fn build_test_server<F>(factory: F) -> TestServer
where
    F: FnOnce(ServerBuilder) -> ServerBuilder + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    // run server in separate thread
    thread::spawn(move || {
        let sys = System::new("ntex-test-server");
        let system = sys.system();

        sys.run(|| {
            let server = factory(super::build()).workers(1).disable_signals().run();
            tx.send((system, server))
                .expect("Failed to send Server to TestServer");
            Ok(())
        })
    });
    let (system, server) = rx.recv().unwrap();

    TestServer {
        system,
        server,
        addr: "127.0.0.1:0".parse().unwrap(),
    }
}

#[derive(Debug)]
/// Test server controller
pub struct TestServer {
    addr: net::SocketAddr,
    system: System,
    server: Server,
}

impl TestServer {
    /// Test server socket addr
    pub fn addr(&self) -> net::SocketAddr {
        self.addr
    }

    pub fn set_addr(mut self, addr: net::SocketAddr) -> Self {
        self.addr = addr;
        self
    }

    /// Connect to server, return Io
    pub async fn connect(&self) -> io::Result<Io> {
        tcp_connect(self.addr).await
    }

    /// Stop http server by stopping the runtime.
    pub fn stop(&self) {
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

    /// Get access to the running Server
    pub fn server(&self) -> Server {
        self.server.clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop()
    }
}
