//! Test server
use std::{fmt, io, marker::PhantomData, net, thread};

use ntex_io::{Io, IoConfig};
use ntex_net::tcp_connect;
use ntex_rt::System;
use ntex_service::ServiceFactory;
use socket2::{Domain, SockAddr, Socket, Type};

use super::{Server, ServerBuilder};

/// Test server builder
pub struct TestServerBuilder<F, R> {
    factory: F,
    config: IoConfig,
    client_config: IoConfig,
    _t: PhantomData<R>,
}

impl<F, R> fmt::Debug for TestServerBuilder<F, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestServerBuilder")
            .field("config", &self.config)
            .field("client_config", &self.client_config)
            .finish()
    }
}

impl<F, R> TestServerBuilder<F, R>
where
    F: Fn() -> R + Send + Clone + 'static,
    R: ServiceFactory<Io> + 'static,
{
    pub fn new(factory: F) -> Self {
        Self {
            factory,
            config: IoConfig::new("TEST-SERVER"),
            client_config: IoConfig::new("TEST-CLIENT"),
            _t: PhantomData,
        }
    }

    /// Set server io configuration
    pub fn config(mut self, cfg: IoConfig) -> Self {
        self.config = cfg;
        self
    }

    /// Set client io configuration
    pub fn client_config(mut self, cfg: IoConfig) -> Self {
        self.client_config = cfg;
        self
    }

    /// Start test server
    pub fn start(self) -> TestServer {
        let factory = self.factory;
        let config = self.config;

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
                    .set_config("test", config)
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
            cfg: self.client_config,
        }
    }
}

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
    TestServerBuilder::new(factory).start()
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
        cfg: IoConfig::new("TEST-CLIENT"),
    }
}

#[derive(Debug)]
/// Test server controller
pub struct TestServer {
    addr: net::SocketAddr,
    system: System,
    server: Server,
    cfg: IoConfig,
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
        tcp_connect(self.addr, self.cfg).await
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
