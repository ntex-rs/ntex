//! Test server
#![allow(clippy::missing_panics_doc)]
use std::{fmt, io, marker::PhantomData, net, thread, time};

use ntex_io::{Io, IoConfig};
use ntex_net::tcp_connect;
use ntex_rt::System;
use ntex_service::{ServiceFactory, cfg::SharedCfg};
use socket2::{Domain, SockAddr, Socket, Type};
use uuid::Uuid;

use super::{Server, ServerBuilder};

/// Test server builder
pub struct TestServerBuilder<F, R> {
    id: Uuid,
    factory: F,
    config: SharedCfg,
    client_config: SharedCfg,
    _t: PhantomData<R>,
}

impl<F, R> fmt::Debug for TestServerBuilder<F, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestServerBuilder")
            .field("id", &self.id)
            .field("config", &self.config)
            .field("client_config", &self.client_config)
            .finish()
    }
}

impl<F, R> TestServerBuilder<F, R>
where
    F: AsyncFn() -> R + Send + Clone + 'static,
    R: ServiceFactory<Io, SharedCfg> + 'static,
{
    #[must_use]
    /// Create test server builder
    pub fn new(factory: F) -> Self {
        Self {
            factory,
            id: Uuid::now_v7(),
            config: SharedCfg::new("TEST-SERVER").into(),
            client_config: SharedCfg::new("TEST-CLIENT").into(),
            _t: PhantomData,
        }
    }

    #[must_use]
    /// Set server io configuration
    pub fn config<T: Into<SharedCfg>>(mut self, cfg: T) -> Self {
        self.config = cfg.into();
        self
    }

    #[must_use]
    /// Set client io configuration
    pub fn client_config<T: Into<SharedCfg>>(mut self, cfg: T) -> Self {
        self.client_config = cfg.into();
        self
    }

    /// Start test server
    pub fn start(self) -> TestServer {
        log::debug!("Starting test server {:?}", self.id);
        let config = self.config;
        let factory = self.factory;
        let cfg = System::current().config();
        let name = System::current().name().to_string();

        let (tx, rx) = oneshot::channel();
        // run server in separate thread
        thread::spawn(move || {
            let sys = System::with_config(&name, cfg);
            let tcp = net::TcpListener::bind("127.0.0.1:0").unwrap();
            let local_addr = tcp.local_addr().unwrap();
            let system = sys.system();

            sys.run(move || {
                let server = Server::builder()
                    .listen("test", tcp, async move |_| factory().await)?
                    .config("test", config)
                    .workers(1)
                    .disable_signals()
                    .enable_affinity()
                    .run();

                ntex_rt::spawn(async move {
                    tx.send((system, local_addr, server))
                        .expect("Failed to send Server to TestServer");
                });

                Ok(())
            })
        });
        let (system, addr, server) = rx.recv().unwrap();
        thread::sleep(time::Duration::from_millis(25));

        TestServer {
            addr,
            server,
            system,
            id: self.id,
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
/// use ntex::{server, http, client::Client};
/// use ntex::web::{self, App, HttpResponse};
///
/// async fn my_handler() -> Result<HttpResponse, std::io::Error> {
///     Ok(HttpResponse::Ok().into())
/// }
///
/// #[ntex::test]
/// async fn test_example() {
///     let mut srv = server::test_server(
///         async || http::HttpService::new(
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
    F: AsyncFn() -> R + Send + Clone + 'static,
    R: ServiceFactory<Io, SharedCfg> + 'static,
{
    TestServerBuilder::new(factory).start()
}

/// Start new server with server builder
pub fn build_test_server<F>(factory: F) -> TestServer
where
    F: AsyncFnOnce(ServerBuilder) -> ServerBuilder + Send + 'static,
{
    let cfg = System::current().config();
    let name = System::current().name().to_string();

    let id = Uuid::now_v7();
    log::debug!("Starting {name:?} server {id:?}");

    let (tx, rx) = oneshot::channel();

    // run server in separate thread
    thread::spawn(move || {
        let sys = System::with_config(&name, cfg);
        let system = sys.system();

        sys.block_on(async move {
            let server = factory(super::build())
                .await
                .workers(1)
                .disable_signals()
                .run();
            tx.send((system, server.clone()))
                .expect("Failed to send Server to TestServer");
            let _ = server.await;
        });
    });
    let (system, server) = rx.recv().unwrap();
    thread::sleep(time::Duration::from_millis(25));

    TestServer {
        id,
        system,
        server,
        addr: "127.0.0.1:0".parse().unwrap(),
        cfg: SharedCfg::new("TEST-CLIENT").add(IoConfig::new()).into(),
    }
}

#[derive(Debug)]
/// Test server controller
pub struct TestServer {
    id: Uuid,
    addr: net::SocketAddr,
    system: System,
    server: Server,
    cfg: SharedCfg,
}

impl TestServer {
    /// Test server socket addr
    pub fn addr(&self) -> net::SocketAddr {
        self.addr
    }

    #[must_use]
    pub fn set_addr(mut self, addr: net::SocketAddr) -> Self {
        self.addr = addr;
        self
    }

    /// Test client shared config
    pub fn config(&self) -> SharedCfg {
        self.cfg.clone()
    }

    /// Connect to server, return Io
    pub async fn connect(&self) -> io::Result<Io> {
        tcp_connect(self.addr, self.cfg.clone()).await
    }

    /// Stop http server by stopping the runtime.
    pub fn stop(&self) {
        drop(self.server.stop(true));
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
        log::debug!("Stopping test server {:?}", self.id);
        drop(self.server.stop(false));
        thread::sleep(time::Duration::from_millis(75));
        self.system.stop();
        thread::sleep(time::Duration::from_millis(25));
    }
}
