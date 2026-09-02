//! Test helpers to use during testing.
use std::{net, str::FromStr, sync::mpsc, thread, time};

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use ntex_tls::TlsConfig;
use uuid::Uuid;

use crate::channel::bstream;
use crate::client::{Client, ClientRequest, ClientResponse, error::ClientPayloadError};
use crate::error::Error;
#[cfg(feature = "ws")]
use crate::io::Filter;
use crate::io::{Io, IoConfig};
use crate::server::{NoConfig, Server};
use crate::service::{IntoService, Service, cfg::SharedCfg};
#[cfg(feature = "ws")]
use crate::ws::{WsClient, WsClientConfig, WsConnection, error::WsClientError};
use crate::{rt::System, time::Millis, time::Seconds, util::Bytes};

use super::header::{self, HeaderMap, HeaderName, HeaderValue};
use super::{Method, Request, Uri, Version, error::HttpError, payload::Payload};

#[derive(Debug)]
/// Test `Request` builder
///
/// ```rust,no_run
/// use ntex::http::test::TestRequest;
/// use ntex::http::{header, Request, Response, StatusCode, HttpMessage};
///
/// fn index(req: Request) -> Response {
///     if let Some(hdr) = req.headers().get(header::CONTENT_TYPE) {
///         Response::Ok().into()
///     } else {
///         Response::BadRequest().into()
///     }
/// }
///
/// let resp = index(
///     TestRequest::with_header("content-type", "text/plain").finish());
/// assert_eq!(resp.status(), StatusCode::OK);
///
/// let resp = index(
///     TestRequest::default().finish());
/// assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
/// ```
pub struct TestRequest(Option<Inner>);

#[derive(Debug)]
struct Inner {
    version: Version,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    #[cfg(feature = "cookie")]
    cookies: CookieJar,
    payload: Option<Payload>,
}

impl Default for TestRequest {
    fn default() -> TestRequest {
        TestRequest(Some(Inner {
            method: Method::GET,
            uri: Uri::from_str("/").unwrap(),
            version: Version::HTTP_11,
            headers: HeaderMap::new(),
            #[cfg(feature = "cookie")]
            cookies: CookieJar::new(),
            payload: None,
        }))
    }
}

impl TestRequest {
    #[must_use]
    /// Create `TestRequest` and set request uri.
    pub fn with_uri(path: &str) -> TestRequest {
        TestRequest::default().uri(path).take()
    }

    #[must_use]
    /// Create `TestRequest` and set header.
    pub fn with_header<K, V>(key: K, value: V) -> TestRequest
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
    {
        TestRequest::default().header(key, value).take()
    }

    /// Set HTTP version of this request.
    pub fn version(&mut self, ver: Version) -> &mut Self {
        parts(&mut self.0).version = ver;
        self
    }

    /// Set HTTP method of this request.
    pub fn method(&mut self, meth: Method) -> &mut Self {
        parts(&mut self.0).method = meth;
        self
    }

    /// Set HTTP Uri of this request.
    pub fn uri(&mut self, path: &str) -> &mut Self {
        parts(&mut self.0).uri = Uri::from_str(path).unwrap();
        self
    }

    /// Set a header.
    pub fn header<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
    {
        if let Ok(key) = HeaderName::try_from(key)
            && let Ok(value) = HeaderValue::try_from(value)
        {
            parts(&mut self.0).headers.append(key, value);
            return self;
        }
        panic!("Cannot create header");
    }

    #[cfg(feature = "cookie")]
    /// Set cookie for this request.
    pub fn cookie<C>(&mut self, cookie: C) -> &mut Self
    where
        C: Into<Cookie<'static>>,
    {
        parts(&mut self.0).cookies.add(cookie.into());
        self
    }

    /// Set request payload.
    pub fn set_payload<B: Into<Bytes>>(&mut self, data: B) -> &mut Self {
        let payload = bstream::empty(Some(data.into()));
        parts(&mut self.0).payload = Some(payload.into());
        self
    }

    #[must_use]
    /// Take test request.
    pub fn take(&mut self) -> TestRequest {
        TestRequest(self.0.take())
    }

    #[must_use]
    /// Complete request creation and generate `Request` instance.
    pub fn finish(&mut self) -> Request {
        let inner = self.0.take().expect("cannot reuse test request builder");

        let mut req = if let Some(pl) = inner.payload {
            Request::with_payload(pl)
        } else {
            Request::with_payload(bstream::empty(None).into())
        };

        let head = req.head_mut();
        head.uri = inner.uri;
        head.method = inner.method;
        head.version = inner.version;
        head.headers = inner.headers;

        if let Some(conn) = head.headers.get(header::CONNECTION)
            && let Ok(s) = conn.to_str()
            && s.to_lowercase().contains("upgrade")
        {
            head.set_upgrade();
        }

        #[cfg(feature = "cookie")]
        {
            use percent_encoding::percent_encode;
            use std::fmt::Write as FmtWrite;

            let mut cookie = String::new();
            for c in inner.cookies.delta() {
                let name = percent_encode(c.name().as_bytes(), super::helpers::USERINFO);
                let value = percent_encode(c.value().as_bytes(), super::helpers::USERINFO);
                let _ = write!(cookie, "; {name}={value}");
            }
            if !cookie.is_empty() {
                head.headers.insert(
                    super::header::COOKIE,
                    HeaderValue::from_str(&cookie.as_str()[2..]).unwrap(),
                );
            }
        }

        req
    }
}

#[inline]
fn parts(parts: &mut Option<Inner>) -> &mut Inner {
    parts.as_mut().expect("cannot reuse test request builder")
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
/// use ntex::web::{self, App, HttpResponse};
///
/// async fn my_handler() -> Result<HttpResponse, std::io::Error> {
///     Ok(HttpResponse::Ok().into())
/// }
///
/// #[ntex::test]
/// async fn test_example() {
///     let mut srv = http::test::server(
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
pub fn server<F, S, I>(factory: F) -> TestServer
where
    F: AsyncFn(&()) -> I + Send + Clone + 'static,
    S: Service<(), Io> + 'static,
    I: IntoService<S, (), Io> + 'static,
{
    server_with_config::<_, _, _>(
        factory,
        SharedCfg::new("HTTP-TEST-SRV")
            .add(IoConfig::new())
            .add(TlsConfig::new())
            .add(ntex_h2::ServiceConfig::new()),
    )
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
/// use ntex::web::{self, App, HttpResponse};
///
/// async fn my_handler() -> Result<HttpResponse, std::io::Error> {
///     Ok(HttpResponse::Ok().into())
/// }
///
/// #[ntex::test]
/// async fn test_example() {
///     let mut srv = http::test::server(
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
pub fn server_with_config<F, S, I>(f: F, cfg: impl Into<SharedCfg>) -> TestServer
where
    F: AsyncFn(&()) -> I + Send + Clone + 'static,
    S: Service<(), Io> + 'static,
    I: IntoService<S, (), Io> + 'static,
{
    let sys = System::current().config();
    let name = System::current().name().to_string();

    let id = Uuid::now_v7();
    let cfg = cfg.into();
    let (tx, rx) = mpsc::channel();
    log::debug!("Starting {name:?} http server {id:?}");

    // run server in separate thread
    thread::spawn(move || {
        let sys = System::with_config(&name, sys);
        let tcp = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local_addr = tcp.local_addr().unwrap();

        sys.run(move || {
            let srv = crate::server::ServerBuilder::new(NoConfig)
                .listen("test", tcp, cfg, async move |st| f(st).await)?
                .workers(1)
                .disable_signals()
                .run();

            crate::rt::spawn(async move {
                tx.send((System::current(), srv, local_addr)).unwrap();
            });
            Ok(())
        })
    });
    let (system, server, addr) = rx.recv().unwrap();
    thread::sleep(Millis(25).into());

    TestServer::create(id, system, server, addr, Seconds(90), Millis(90_000))
}

#[derive(Debug)]
/// Test server controller
pub struct TestServer {
    id: Uuid,
    cfg: SharedCfg,
    addr: net::SocketAddr,
    client: Client,
    system: System,
    server: Server,
}

impl TestServer {
    pub fn create(
        id: Uuid,
        system: System,
        server: Server,
        addr: net::SocketAddr,
        timeout: Seconds,
        connect_timeout: Millis,
    ) -> Self {
        let cfg = SharedCfg::new("TEST-CLIENT")
            .add(IoConfig::new().set_connect_timeout(connect_timeout))
            .add(TlsConfig::new().set_handshake_timeout(timeout))
            .add(
                ntex_h2::ServiceConfig::new()
                    .set_max_header_list_size(256 * 1024)
                    .set_max_header_continuation_frames(96),
            );
        #[cfg(feature = "ws")]
        let cfg = cfg.add(
            WsClientConfig::new()
                .set_address(addr)
                .set_timeout(Seconds(30)),
        );
        let cfg = cfg.build();

        let client = Self::create_client(cfg.clone());

        TestServer {
            id,
            cfg,
            addr,
            client,
            system,
            server,
        }
    }

    #[must_use]
    /// Set client timeout
    pub fn set_client_timeout(mut self, timeout: Seconds, connect_timeout: Millis) -> Self {
        let cfg = SharedCfg::new("TEST-CLIENT")
            .add(IoConfig::new().set_connect_timeout(connect_timeout))
            .add(TlsConfig::new().set_handshake_timeout(timeout))
            .add(
                ntex_h2::ServiceConfig::new()
                    .set_max_header_list_size(256 * 1024)
                    .set_max_header_continuation_frames(96),
            );
        #[cfg(feature = "ws")]
        let cfg = cfg.add(
            WsClientConfig::new()
                .set_address(self.addr)
                .set_timeout(Seconds(30)),
        );
        self.cfg = cfg.build();
        self.client = Self::create_client(self.cfg.clone());
        self
    }

    /// Set client timeout
    fn create_client(cfg: SharedCfg) -> Client {
        #[cfg(feature = "openssl")]
        {
            use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

            let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
            builder.set_verify(SslVerifyMode::NONE);
            let _ = builder
                .set_alpn_protos(b"\x02h2\x08http/1.1")
                .map_err(|e| log::error!("Cannot set alpn protocol: {e:?}"));
            Client::builder().openssl(builder.build()).build(cfg)
        }
        #[cfg(not(feature = "openssl"))]
        {
            Client::builder().build(cfg)
        }
    }

    /// Construct test server url
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

    /// Create client request
    pub fn request<S: AsRef<str>>(&self, method: Method, path: S) -> ClientRequest {
        self.client
            .request(method, self.url(path.as_ref()).as_str())
    }

    /// Create secure client request
    pub fn srequest<S: AsRef<str>>(&self, method: Method, path: S) -> ClientRequest {
        self.client
            .request(method, self.surl(path.as_ref()).as_str())
    }

    /// Load response's body
    pub async fn load_body(
        &self,
        response: ClientResponse,
    ) -> Result<Bytes, Error<ClientPayloadError>> {
        response.body().limit(10_485_760).await
    }

    #[cfg(feature = "ws")]
    /// Connect to a websocket server
    pub async fn ws(&self) -> Result<WsConnection<impl Filter>, Error<WsClientError>> {
        self.ws_at("/").await
    }

    #[cfg(feature = "ws")]
    /// Connect to websocket server at a given path
    pub async fn ws_at(
        &self,
        path: &str,
    ) -> Result<WsConnection<impl Filter>, Error<WsClientError>> {
        WsClient::new(self.url(path), &self.cfg)
            .unwrap()
            .connect()
            .await
    }

    #[cfg(all(feature = "openssl", feature = "ws"))]
    /// Connect to a websocket server
    pub async fn wss(
        &self,
    ) -> Result<
        WsConnection<crate::io::Layer<crate::connect::openssl::SslFilter>>,
        Error<WsClientError>,
    > {
        self.wss_at("/").await
    }

    #[cfg(all(feature = "openssl", feature = "ws"))]
    /// Connect to secure websocket server at a given path
    pub async fn wss_at(
        &self,
        path: &str,
    ) -> Result<
        WsConnection<crate::io::Layer<crate::connect::openssl::SslFilter>>,
        Error<WsClientError>,
    > {
        use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

        let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
        builder.set_verify(SslVerifyMode::NONE);
        let _ = builder
            .set_alpn_protos(b"\x08http/1.1")
            .map_err(|e| log::error!("Cannot set alpn protocol: {e:?}"));

        WsClient::new(self.url(path), &self.cfg)
            .unwrap()
            .openssl(builder.build())
            .connect()
            .await
    }

    #[cfg(all(
        windows,
        feature = "schannel",
        feature = "ws",
        not(feature = "openssl")
    ))]
    /// Connect to a websocket server
    pub async fn wss(
        &self,
    ) -> Result<
        WsConnection<crate::io::Layer<crate::connect::schannel::SchannelFilter>>,
        Error<WsClientError>,
    > {
        self.wss_at("/").await
    }

    #[cfg(all(
        windows,
        feature = "schannel",
        feature = "ws",
        not(feature = "openssl")
    ))]
    /// Connect to secure websocket server at a given path
    pub async fn wss_at(
        &self,
        path: &str,
    ) -> Result<
        WsConnection<crate::io::Layer<crate::connect::schannel::SchannelFilter>>,
        Error<WsClientError>,
    > {
        WsClient::new(self.url(path), &self.cfg)
            .unwrap()
            .schannel(
                crate::connect::schannel::ClientConfig::new().danger_accept_invalid_certs(true),
            )
            .connect()
            .await
    }

    /// Stop http server
    pub async fn stop(self, graceful: bool) {
        self.server.stop(graceful).await;
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        log::debug!("Stopping test http server {:?}", self.id);
        drop(self.server.stop(false));
        thread::sleep(time::Duration::from_millis(75));
        self.system.stop();
        thread::sleep(time::Duration::from_millis(25));
    }
}
