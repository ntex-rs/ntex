//! Test helpers to use during testing.
use std::{net, str::FromStr, sync::mpsc, thread, time};

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use ntex_tls::TlsConfig;

use crate::channel::bstream;
use crate::client::{Client, ClientBuilder, ClientRequest, ClientResponse, Connector};
#[cfg(feature = "ws")]
use crate::io::Filter;
use crate::io::{Io, IoConfig};
use crate::server::Server;
use crate::service::{ServiceFactory, cfg::SharedCfg};
#[cfg(feature = "ws")]
use crate::ws::{WsClient, WsConnection, error::WsClientError};
use crate::{rt::System, time::Millis, time::Seconds, time::sleep, util::Bytes};

use super::error::{HttpError, PayloadError};
use super::header::{self, HeaderMap, HeaderName, HeaderValue};
use super::payload::Payload;
use super::{Method, Request, Uri, Version};

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
    /// Create TestRequest and set request uri
    pub fn with_uri(path: &str) -> TestRequest {
        TestRequest::default().uri(path).take()
    }

    /// Create TestRequest and set header
    pub fn with_header<K, V>(key: K, value: V) -> TestRequest
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
    {
        TestRequest::default().header(key, value).take()
    }

    /// Set HTTP version of this request
    pub fn version(&mut self, ver: Version) -> &mut Self {
        parts(&mut self.0).version = ver;
        self
    }

    /// Set HTTP method of this request
    pub fn method(&mut self, meth: Method) -> &mut Self {
        parts(&mut self.0).method = meth;
        self
    }

    /// Set HTTP Uri of this request
    pub fn uri(&mut self, path: &str) -> &mut Self {
        parts(&mut self.0).uri = Uri::from_str(path).unwrap();
        self
    }

    /// Set a header
    pub fn header<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
    {
        if let Ok(key) = HeaderName::try_from(key) {
            if let Ok(value) = HeaderValue::try_from(value) {
                parts(&mut self.0).headers.append(key, value);
                return self;
            }
        }
        panic!("Cannot create header");
    }

    #[cfg(feature = "cookie")]
    /// Set cookie for this request
    pub fn cookie<C>(&mut self, cookie: C) -> &mut Self
    where
        C: Into<Cookie<'static>>,
    {
        parts(&mut self.0).cookies.add(cookie.into());
        self
    }

    /// Set request payload
    pub fn set_payload<B: Into<Bytes>>(&mut self, data: B) -> &mut Self {
        let payload = bstream::empty(Some(data.into()));
        parts(&mut self.0).payload = Some(payload.into());
        self
    }

    /// Take test request
    pub fn take(&mut self) -> TestRequest {
        TestRequest(self.0.take())
    }

    /// Complete request creation and generate `Request` instance
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

        if let Some(conn) = head.headers.get(header::CONNECTION) {
            if let Ok(s) = conn.to_str() {
                if s.to_lowercase().contains("upgrade") {
                    head.set_upgrade()
                }
            }
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
pub async fn server<F, R>(factory: F) -> TestServer
where
    F: AsyncFn() -> R + Send + Clone + 'static,
    R: ServiceFactory<Io, SharedCfg> + 'static,
{
    server_with_config(factory, SharedCfg::new("HTTP-TEST-SRV")).await
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
pub async fn server_with_config<F, R, U>(factory: F, cfg: U) -> TestServer
where
    F: AsyncFn() -> R + Send + Clone + 'static,
    R: ServiceFactory<Io, SharedCfg> + 'static,
    U: Into<SharedCfg>,
{
    let cfg = cfg.into();
    let (tx, rx) = mpsc::channel();

    // run server in separate thread
    thread::spawn(move || {
        let sys = System::new("test-server");
        let tcp = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local_addr = tcp.local_addr().unwrap();

        let system = sys.system();
        sys.run(move || {
            let srv = crate::server::build()
                .listen("test", tcp, async move |_| factory().await)?
                .config("test", cfg)
                .workers(1)
                .disable_signals()
                .run();

            crate::rt::spawn(async move {
                tx.send((system, srv, local_addr)).unwrap();
            });
            Ok(())
        })
    });
    let (system, server, addr) = rx.recv().unwrap();
    sleep(Millis(50)).await;

    TestServer {
        addr,
        system,
        server,
        client: ClientBuilder::new()
            .build(SharedCfg::default())
            .await
            .unwrap(),
    }
    .set_client_timeout(Seconds(90), Millis(90_000))
    .await
}

#[derive(Debug)]
/// Test server controller
pub struct TestServer {
    addr: net::SocketAddr,
    client: Client,
    system: System,
    server: Server,
}

impl TestServer {
    /// Set client timeout
    pub async fn set_client_timeout(
        mut self,
        timeout: Seconds,
        connect_timeout: Millis,
    ) -> Self {
        let cfg: SharedCfg = SharedCfg::new("TEST-CLIENT")
            .add(IoConfig::new().set_connect_timeout(connect_timeout))
            .add(TlsConfig::new().set_handshake_timeout(timeout))
            .add(
                ntex_h2::ServiceConfig::new()
                    .set_max_header_list_size(256 * 1024)
                    .set_max_header_continuation_frames(96),
            )
            .into();

        let client = {
            let connector = {
                #[cfg(feature = "openssl")]
                {
                    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

                    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
                    builder.set_verify(SslVerifyMode::NONE);
                    let _ = builder
                        .set_alpn_protos(b"\x02h2\x08http/1.1")
                        .map_err(|e| log::error!("Cannot set alpn protocol: {e:?}"));
                    Connector::default().openssl(builder.build())
                }
                #[cfg(not(feature = "openssl"))]
                {
                    Connector::default()
                }
            };

            Client::builder()
                .connector::<&str>(connector)
                .build(cfg)
                .await
                .unwrap()
        };

        self.client = client;
        self
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
        &mut self,
        mut response: ClientResponse,
    ) -> Result<Bytes, PayloadError> {
        response.body().limit(10_485_760).await
    }

    #[cfg(feature = "ws")]
    /// Connect to a websocket server
    pub async fn ws(&mut self) -> Result<WsConnection<impl Filter>, WsClientError> {
        self.ws_at("/").await
    }

    #[cfg(feature = "ws")]
    /// Connect to websocket server at a given path
    pub async fn ws_at(
        &mut self,
        path: &str,
    ) -> Result<WsConnection<impl Filter>, WsClientError> {
        WsClient::build(self.url(path))
            .address(self.addr)
            .timeout(Seconds(30))
            .finish(SharedCfg::default())
            .await
            .unwrap()
            .connect()
            .await
    }

    #[cfg(all(feature = "openssl", feature = "ws"))]
    /// Connect to a websocket server
    pub async fn wss(
        &mut self,
    ) -> Result<
        WsConnection<crate::io::Layer<crate::connect::openssl::SslFilter>>,
        WsClientError,
    > {
        self.wss_at("/").await
    }

    #[cfg(all(feature = "openssl", feature = "ws"))]
    /// Connect to secure websocket server at a given path
    pub async fn wss_at(
        &mut self,
        path: &str,
    ) -> Result<
        WsConnection<crate::io::Layer<crate::connect::openssl::SslFilter>>,
        WsClientError,
    > {
        use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

        let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
        builder.set_verify(SslVerifyMode::NONE);
        let _ = builder
            .set_alpn_protos(b"\x08http/1.1")
            .map_err(|e| log::error!("Cannot set alpn protocol: {e:?}"));

        WsClient::build(self.url(path))
            .address(self.addr)
            .timeout(Seconds(30))
            .openssl(builder.build())
            .take()
            .finish(SharedCfg::default())
            .await
            .unwrap()
            .connect()
            .await
    }

    /// Stop http server
    pub async fn stop(self) {
        self.server.stop(true).await;
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        thread::sleep(time::Duration::from_millis(100));
        let _ = self.server.stop(true);
        thread::sleep(time::Duration::from_millis(75));
        self.system.stop();
        thread::sleep(time::Duration::from_millis(25));
    }
}
