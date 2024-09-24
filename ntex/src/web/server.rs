use std::{fmt, io, marker::PhantomData, net, sync::Arc, sync::Mutex};

#[cfg(feature = "openssl")]
use tls_openssl::ssl::{AlpnError, SslAcceptor, SslAcceptorBuilder};
#[cfg(feature = "rustls")]
use tls_rustls::ServerConfig as RustlsServerConfig;

use crate::http::{
    self, body::MessageBody, HttpService, KeepAlive, Request, Response, ResponseError,
};
use crate::server::{Server, ServerBuilder};
use crate::service::{map_config, IntoServiceFactory, ServiceFactory};
use crate::{time::Seconds, util::PoolId};

use super::config::AppConfig;

struct Config {
    host: Option<String>,
    keep_alive: KeepAlive,
    client_disconnect: Seconds,
    ssl_handshake_timeout: Seconds,
    headers_read_rate: Option<ReadRate>,
    payload_read_rate: Option<ReadRate>,
    tag: &'static str,
    pool: PoolId,
}

#[derive(Default, Copy, Clone)]
struct ReadRate {
    rate: u16,
    timeout: Seconds,
    max_timeout: Seconds,
}

impl Config {
    #[allow(clippy::wrong_self_convention)]
    fn into_cfg(&self) -> http::ServiceConfig {
        let mut svc_cfg = http::ServiceConfig::default();
        svc_cfg.keepalive(self.keep_alive);
        svc_cfg.disconnect_timeout(self.client_disconnect);
        svc_cfg.ssl_handshake_timeout(self.ssl_handshake_timeout);
        if let Some(hdrs) = self.headers_read_rate {
            svc_cfg.headers_read_rate(hdrs.timeout, hdrs.max_timeout, hdrs.rate);
        }
        if let Some(hdrs) = self.payload_read_rate {
            svc_cfg.payload_read_rate(hdrs.timeout, hdrs.max_timeout, hdrs.rate);
        }
        svc_cfg
    }
}

/// An HTTP Server.
///
/// Create new http server with application factory.
///
/// ```rust,no_run
/// use ntex::web::{self, App, HttpResponse, HttpServer};
///
/// #[ntex::main]
/// async fn main() -> std::io::Result<()> {
///     HttpServer::new(
///         || App::new()
///             .service(web::resource("/").to(|| async { HttpResponse::Ok() })))
///         .bind("127.0.0.1:59090")?
///         .run()
///         .await
/// }
/// ```
pub struct HttpServer<F, I, S, B>
where
    F: Fn() -> I + Send + Clone + 'static,
    I: IntoServiceFactory<S, Request, AppConfig>,
    S: ServiceFactory<Request, AppConfig>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    pub(super) factory: F,
    config: Arc<Mutex<Config>>,
    backlog: i32,
    builder: ServerBuilder,
    _t: PhantomData<(S, B)>,
}

impl<F, I, S, B> HttpServer<F, I, S, B>
where
    F: Fn() -> I + Send + Clone + 'static,
    I: IntoServiceFactory<S, Request, AppConfig>,
    S: ServiceFactory<Request, AppConfig> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody + 'static,
{
    /// Create new http server with application factory
    pub fn new(factory: F) -> Self {
        HttpServer {
            factory,
            config: Arc::new(Mutex::new(Config {
                host: None,
                keep_alive: KeepAlive::Timeout(Seconds(5)),
                client_disconnect: Seconds(1),
                ssl_handshake_timeout: Seconds(5),
                headers_read_rate: Some(ReadRate {
                    rate: 256,
                    timeout: Seconds(1),
                    max_timeout: Seconds(13),
                }),
                payload_read_rate: None,
                tag: "WEB",
                pool: PoolId::P0,
            })),
            backlog: 1024,
            builder: ServerBuilder::default(),
            _t: PhantomData,
        }
    }

    /// Set number of workers to start.
    ///
    /// By default http server uses number of available logical cpu as threads
    /// count.
    pub fn workers(mut self, num: usize) -> Self {
        self.builder = self.builder.workers(num);
        self
    }

    /// Set the maximum number of pending connections.
    ///
    /// This refers to the number of clients that can be waiting to be served.
    /// Exceeding this number results in the client getting an error when
    /// attempting to connect. It should only affect servers under significant
    /// load.
    ///
    /// Generally set in the 64-2048 range. Default value is 2048.
    ///
    /// This method should be called before `bind()` method call.
    pub fn backlog(mut self, backlog: i32) -> Self {
        self.backlog = backlog;
        self.builder = self.builder.backlog(backlog);
        self
    }

    /// Sets the maximum per-worker number of concurrent connections.
    ///
    /// All socket listeners will stop accepting connections when this limit is reached
    /// for each worker.
    ///
    /// By default max connections is set to a 25k.
    pub fn maxconn(mut self, num: usize) -> Self {
        self.builder = self.builder.maxconn(num);
        self
    }

    /// Sets the maximum per-worker concurrent connection establish process.
    ///
    /// All listeners will stop accepting connections when this limit is reached. It
    /// can be used to limit the global SSL CPU usage.
    ///
    /// By default max connections is set to a 256.
    pub fn maxconnrate(self, num: usize) -> Self {
        ntex_tls::max_concurrent_ssl_accept(num);
        self
    }

    /// Set server keep-alive setting.
    ///
    /// By default keep alive is set to a 5 seconds.
    pub fn keep_alive<T: Into<KeepAlive>>(self, val: T) -> Self {
        self.config.lock().unwrap().keep_alive = val.into();
        self
    }

    /// Set request read timeout in seconds.
    ///
    /// Defines a timeout for reading client request headers. If a client does not transmit
    /// the entire set headers within this time, the request is terminated with
    /// the 408 (Request Time-out) error.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default client timeout is set to 3 seconds.
    pub fn client_timeout(self, timeout: Seconds) -> Self {
        {
            let mut cfg = self.config.lock().unwrap();

            if timeout.is_zero() {
                cfg.headers_read_rate = None;
            } else {
                let mut rate = cfg.headers_read_rate.unwrap_or_default();
                rate.timeout = timeout;
                cfg.headers_read_rate = Some(rate);
            }
        }
        self
    }

    /// Set server connection disconnect timeout in seconds.
    ///
    /// Defines a timeout for shutdown connection. If a shutdown procedure does not complete
    /// within this time, the request is dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default client timeout is set to 5 seconds.
    pub fn disconnect_timeout(self, val: Seconds) -> Self {
        self.config.lock().unwrap().client_disconnect = val;
        self
    }

    /// Set server ssl handshake timeout in seconds.
    ///
    /// Defines a timeout for connection ssl handshake negotiation.
    /// To disable timeout set value to 0.
    ///
    /// By default handshake timeout is set to 5 seconds.
    pub fn ssl_handshake_timeout(self, val: Seconds) -> Self {
        self.config.lock().unwrap().ssl_handshake_timeout = val;
        self
    }

    /// Set read rate parameters for request headers.
    ///
    /// Set max timeout for reading request headers. If the client
    /// sends `rate` amount of data, increase the timeout by 1 second for every.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default headers read rate is set to 1sec with max timeout 5sec.
    pub fn headers_read_rate(
        self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u16,
    ) -> Self {
        if !timeout.is_zero() {
            self.config.lock().unwrap().headers_read_rate = Some(ReadRate {
                rate,
                timeout,
                max_timeout,
            });
        } else {
            self.config.lock().unwrap().headers_read_rate = None;
        }
        self
    }

    /// Set read rate parameters for request's payload.
    ///
    /// Set time pariod for reading payload. Client must
    /// sends `rate` amount of data per one time period.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default payload read rate is disabled.
    pub fn payload_read_rate(
        self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u16,
    ) -> Self {
        if !timeout.is_zero() {
            self.config.lock().unwrap().payload_read_rate = Some(ReadRate {
                rate,
                timeout,
                max_timeout,
            });
        } else {
            self.config.lock().unwrap().payload_read_rate = None;
        }
        self
    }

    /// Set server host name.
    ///
    /// Host name is used by application router as a hostname for url generation.
    /// Check [ConnectionInfo](./dev/struct.ConnectionInfo.html#method.host)
    /// documentation for more information.
    ///
    /// By default host name is set to a "localhost" value.
    pub fn server_hostname<T: AsRef<str>>(self, val: T) -> Self {
        self.config.lock().unwrap().host = Some(val.as_ref().to_owned());
        self
    }

    /// Stop ntex runtime when server get dropped.
    ///
    /// By default "stop runtime" is disabled.
    pub fn stop_runtime(mut self) -> Self {
        self.builder = self.builder.stop_runtime();
        self
    }

    /// Disable signal handling.
    ///
    /// By default signal handling is enabled.
    pub fn disable_signals(mut self) -> Self {
        self.builder = self.builder.disable_signals();
        self
    }

    /// Timeout for graceful workers shutdown.
    ///
    /// After receiving a stop signal, workers have this much time to finish
    /// serving requests. Workers still alive after the timeout are force
    /// dropped.
    ///
    /// By default shutdown timeout sets to 30 seconds.
    pub fn shutdown_timeout(mut self, sec: Seconds) -> Self {
        self.builder = self.builder.shutdown_timeout(sec);
        self
    }

    /// Set io tag for web server
    pub fn tag(self, tag: &'static str) -> Self {
        self.config.lock().unwrap().tag = tag;
        self
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations.
    pub fn memory_pool(self, id: PoolId) -> Self {
        self.config.lock().unwrap().pool = id;
        self
    }

    /// Use listener for accepting incoming connection requests
    ///
    /// HttpServer does not change any configuration for TcpListener,
    /// it needs to be configured before passing it to listen() method.
    pub fn listen(mut self, lst: net::TcpListener) -> io::Result<Self> {
        let cfg = self.config.clone();
        let factory = self.factory.clone();
        let addr = lst.local_addr().unwrap();

        self.builder =
            self.builder
                .listen(format!("ntex-web-service-{}", addr), lst, move |r| {
                    let c = cfg.lock().unwrap();
                    let cfg = AppConfig::new(
                        false,
                        addr,
                        c.host.clone().unwrap_or_else(|| format!("{}", addr)),
                    );
                    r.tag(c.tag);
                    r.memory_pool(c.pool);

                    HttpService::build_with_config(c.into_cfg())
                        .finish(map_config(factory(), move |_| cfg.clone()))
                })?;
        Ok(self)
    }

    #[cfg(feature = "openssl")]
    /// Use listener for accepting incoming tls connection requests
    ///
    /// This method sets alpn protocols to "h2" and "http/1.1"
    pub fn listen_openssl(
        self,
        lst: net::TcpListener,
        builder: SslAcceptorBuilder,
    ) -> io::Result<Self> {
        self.listen_ssl_inner(lst, openssl_acceptor(builder)?)
    }

    #[cfg(feature = "openssl")]
    fn listen_ssl_inner(
        mut self,
        lst: net::TcpListener,
        acceptor: SslAcceptor,
    ) -> io::Result<Self> {
        let factory = self.factory.clone();
        let cfg = self.config.clone();
        let addr = lst.local_addr().unwrap();

        self.builder =
            self.builder
                .listen(format!("ntex-web-service-{}", addr), lst, move |r| {
                    let c = cfg.lock().unwrap();
                    let cfg = AppConfig::new(
                        true,
                        addr,
                        c.host.clone().unwrap_or_else(|| format!("{}", addr)),
                    );
                    r.tag(c.tag);
                    r.memory_pool(c.pool);

                    HttpService::build_with_config(c.into_cfg())
                        .finish(map_config(factory(), move |_| cfg.clone()))
                        .openssl(acceptor.clone())
                })?;
        Ok(self)
    }

    #[cfg(feature = "rustls")]
    /// Use listener for accepting incoming tls connection requests
    ///
    /// This method sets alpn protocols to "h2" and "http/1.1"
    pub fn listen_rustls(
        self,
        lst: net::TcpListener,
        config: RustlsServerConfig,
    ) -> io::Result<Self> {
        self.listen_rustls_inner(lst, config)
    }

    #[cfg(feature = "rustls")]
    fn listen_rustls_inner(
        mut self,
        lst: net::TcpListener,
        config: RustlsServerConfig,
    ) -> io::Result<Self> {
        let factory = self.factory.clone();
        let cfg = self.config.clone();
        let addr = lst.local_addr().unwrap();

        self.builder = self.builder.listen(
            format!("ntex-web-rustls-service-{}", addr),
            lst,
            move |r| {
                let c = cfg.lock().unwrap();
                let cfg = AppConfig::new(
                    true,
                    addr,
                    c.host.clone().unwrap_or_else(|| format!("{}", addr)),
                );
                r.tag(c.tag);
                r.memory_pool(c.pool);

                HttpService::build_with_config(c.into_cfg())
                    .finish(map_config(factory(), move |_| cfg.clone()))
                    .rustls(config.clone())
            },
        )?;
        Ok(self)
    }

    /// The socket address to bind
    ///
    /// To bind multiple addresses this method can be called multiple times.
    pub fn bind<A: net::ToSocketAddrs>(mut self, addr: A) -> io::Result<Self> {
        let sockets = self.bind2(addr)?;

        for lst in sockets {
            self = self.listen(lst)?;
        }

        Ok(self)
    }

    fn bind2<A: net::ToSocketAddrs>(&self, addr: A) -> io::Result<Vec<net::TcpListener>> {
        let mut err = None;
        let mut succ = false;
        let mut sockets = Vec::new();
        for addr in addr.to_socket_addrs()? {
            match crate::server::create_tcp_listener(addr, self.backlog) {
                Ok(lst) => {
                    succ = true;
                    sockets.push(lst);
                }
                Err(e) => err = Some(e),
            }
        }

        if !succ {
            if let Some(e) = err.take() {
                Err(e)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Cannot bind to address.",
                ))
            }
        } else {
            Ok(sockets)
        }
    }

    #[cfg(feature = "openssl")]
    /// Start listening for incoming tls connections.
    ///
    /// This method sets alpn protocols to "h2" and "http/1.1"
    pub fn bind_openssl<A>(
        mut self,
        addr: A,
        builder: SslAcceptorBuilder,
    ) -> io::Result<Self>
    where
        A: net::ToSocketAddrs,
    {
        let sockets = self.bind2(addr)?;
        let acceptor = openssl_acceptor(builder)?;

        for lst in sockets {
            self = self.listen_ssl_inner(lst, acceptor.clone())?;
        }

        Ok(self)
    }

    #[cfg(feature = "rustls")]
    /// Start listening for incoming tls connections.
    ///
    /// This method sets alpn protocols to "h2" and "http/1.1"
    pub fn bind_rustls<A: net::ToSocketAddrs>(
        mut self,
        addr: A,
        config: RustlsServerConfig,
    ) -> io::Result<Self> {
        let sockets = self.bind2(addr)?;
        for lst in sockets {
            self = self.listen_rustls_inner(lst, config.clone())?;
        }
        Ok(self)
    }

    #[cfg(unix)]
    /// Start listening for unix domain connections on existing listener.
    ///
    /// This method is available with `uds` feature.
    pub fn listen_uds(mut self, lst: std::os::unix::net::UnixListener) -> io::Result<Self> {
        let cfg = self.config.clone();
        let factory = self.factory.clone();
        let socket_addr =
            net::SocketAddr::new(net::IpAddr::V4(net::Ipv4Addr::new(127, 0, 0, 1)), 8080);

        let addr = format!("ntex-web-service-{:?}", lst.local_addr()?);

        self.builder = self.builder.listen_uds(addr, lst, move |r| {
            let c = cfg.lock().unwrap();
            let config = AppConfig::new(
                false,
                socket_addr,
                c.host.clone().unwrap_or_else(|| format!("{}", socket_addr)),
            );
            r.tag(c.tag);
            r.memory_pool(c.pool);

            HttpService::build_with_config(c.into_cfg())
                .finish(map_config(factory(), move |_| config.clone()))
        })?;
        Ok(self)
    }

    #[cfg(unix)]
    /// Start listening for incoming unix domain connections.
    ///
    /// This method is available with `uds` feature.
    pub fn bind_uds<A>(mut self, addr: A) -> io::Result<Self>
    where
        A: AsRef<std::path::Path>,
    {
        let cfg = self.config.clone();
        let factory = self.factory.clone();
        let socket_addr =
            net::SocketAddr::new(net::IpAddr::V4(net::Ipv4Addr::new(127, 0, 0, 1)), 8080);

        self.builder = self.builder.bind_uds(
            format!("ntex-web-service-{:?}", addr.as_ref()),
            addr,
            move |r| {
                let c = cfg.lock().unwrap();
                let config = AppConfig::new(
                    false,
                    socket_addr,
                    c.host.clone().unwrap_or_else(|| format!("{}", socket_addr)),
                );
                r.tag(c.tag);
                r.memory_pool(c.pool);

                HttpService::build_with_config(c.into_cfg())
                    .finish(map_config(factory(), move |_| config.clone()))
            },
        )?;
        Ok(self)
    }
}

impl<F, I, S, B> HttpServer<F, I, S, B>
where
    F: Fn() -> I + Send + Clone + 'static,
    I: IntoServiceFactory<S, Request, AppConfig>,
    S: ServiceFactory<Request, AppConfig>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    S::Service: 'static,
    B: MessageBody,
{
    /// Start listening for incoming connections.
    ///
    /// This method starts number of http workers in separate threads.
    /// For each address this method starts separate thread which does
    /// `accept()` in a loop.
    ///
    /// This methods panics if no socket address can be bound or an ntex system
    /// is not yet configured.
    ///
    /// ```rust,no_run
    /// use ntex::web::{self, App, HttpResponse, HttpServer};
    ///
    /// #[ntex::main]
    /// async fn main() -> std::io::Result<()> {
    ///     HttpServer::new(
    ///         || App::new().service(web::resource("/").to(|| async { HttpResponse::Ok() }))
    ///     )
    ///         .bind("127.0.0.1:0")?
    ///         .run()
    ///         .await
    /// }
    /// ```
    pub fn run(self) -> Server {
        self.builder.run()
    }
}

#[cfg(feature = "openssl")]
/// Configure `SslAcceptorBuilder` with custom server flags.
fn openssl_acceptor(mut builder: SslAcceptorBuilder) -> io::Result<SslAcceptor> {
    builder.set_alpn_select_callback(|_, protos| {
        const H2: &[u8] = b"\x02h2";
        const H11: &[u8] = b"\x08http/1.1";
        if protos.windows(3).any(|window| window == H2) {
            Ok(b"h2")
        } else if protos.windows(9).any(|window| window == H11) {
            Ok(b"http/1.1")
        } else {
            Err(AlpnError::NOACK)
        }
    });
    builder.set_alpn_protos(b"\x08http/1.1\x02h2")?;

    Ok(builder.build())
}
