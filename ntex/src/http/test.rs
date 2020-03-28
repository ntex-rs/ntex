//! Test helpers to use during testing.
use std::convert::TryFrom;
use std::io::{self, Read, Write};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::mpsc;
use std::task::{Context, Poll};
use std::{net, thread, time};

use bytes::{Bytes, BytesMut};
use futures::Stream;

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use crate::codec::{AsyncRead, AsyncWrite, Framed};
use crate::rt::{net::TcpStream, System};
use crate::server::{Server, ServiceFactory};

use super::client::error::WsClientError;
use super::client::{Client, ClientRequest, ClientResponse, Connector};
use super::error::{HttpError, PayloadError};
use super::header::{HeaderMap, HeaderName, IntoHeaderValue};
use super::payload::Payload;
use super::{Method, Request, Uri, Version};

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
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        V: IntoHeaderValue,
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
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        V: IntoHeaderValue,
    {
        if let Ok(key) = HeaderName::try_from(key) {
            if let Ok(value) = value.try_into() {
                parts(&mut self.0).headers.append(key, value);
                return self;
            }
        }
        panic!("Can not create header");
    }

    #[cfg(feature = "cookie")]
    /// Set cookie for this request
    pub fn cookie<'a>(&mut self, cookie: Cookie<'a>) -> &mut Self {
        parts(&mut self.0).cookies.add(cookie.into_owned());
        self
    }

    /// Set request payload
    pub fn set_payload<B: Into<Bytes>>(&mut self, data: B) -> &mut Self {
        let mut payload = crate::http::h1::Payload::empty();
        payload.unread_data(data.into());
        parts(&mut self.0).payload = Some(payload.into());
        self
    }

    pub fn take(&mut self) -> TestRequest {
        TestRequest(self.0.take())
    }

    /// Complete request creation and generate `Request` instance
    pub fn finish(&mut self) -> Request {
        let inner = self.0.take().expect("cannot reuse test request builder");

        let mut req = if let Some(pl) = inner.payload {
            Request::with_payload(pl)
        } else {
            Request::with_payload(crate::http::h1::Payload::empty().into())
        };

        let head = req.head_mut();
        head.uri = inner.uri;
        head.method = inner.method;
        head.version = inner.version;
        head.headers = inner.headers;

        #[cfg(feature = "cookie")]
        {
            use crate::http::header::HeaderValue;
            use percent_encoding::percent_encode;
            use std::fmt::Write as FmtWrite;

            let mut cookie = String::new();
            for c in inner.cookies.delta() {
                let name = percent_encode(c.name().as_bytes(), super::helpers::USERINFO);
                let value =
                    percent_encode(c.value().as_bytes(), super::helpers::USERINFO);
                let _ = write!(&mut cookie, "; {}={}", name, value);
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

/// Async io buffer
pub struct TestBuffer {
    pub read_buf: BytesMut,
    pub write_buf: BytesMut,
    pub err: Option<io::Error>,
}

impl TestBuffer {
    /// Create new TestBuffer instance
    pub fn new<T>(data: T) -> TestBuffer
    where
        BytesMut: From<T>,
    {
        TestBuffer {
            read_buf: BytesMut::from(data),
            write_buf: BytesMut::new(),
            err: None,
        }
    }

    /// Create new empty TestBuffer instance
    pub fn empty() -> TestBuffer {
        TestBuffer::new("")
    }

    /// Add extra data to read buffer.
    pub fn extend_read_buf<T: AsRef<[u8]>>(&mut self, data: T) {
        self.read_buf.extend_from_slice(data.as_ref())
    }
}

impl io::Read for TestBuffer {
    fn read(&mut self, dst: &mut [u8]) -> Result<usize, io::Error> {
        if self.read_buf.is_empty() {
            if self.err.is_some() {
                Err(self.err.take().unwrap())
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
            }
        } else {
            let size = std::cmp::min(self.read_buf.len(), dst.len());
            let b = self.read_buf.split_to(size);
            dst[..size].copy_from_slice(&b);
            Ok(size)
        }
    }
}

impl io::Write for TestBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_buf.extend(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsyncRead for TestBuffer {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.get_mut().read(buf))
    }
}

impl AsyncWrite for TestBuffer {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.get_mut().write(buf))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Start test server
///
/// `TestServer` is very simple test server that simplify process of writing
/// integration tests cases for actix web applications.
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
pub fn server<F: ServiceFactory<TcpStream>>(factory: F) -> TestServer {
    let (tx, rx) = mpsc::channel();

    // run server in separate thread
    thread::spawn(move || {
        let sys = System::new("actix-test-server");
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
    crate::connect::start_default_resolver();

    TestServer {
        addr,
        client,
        system,
    }
}

/// Test server controller
pub struct TestServer {
    addr: net::SocketAddr,
    client: Client,
    system: System,
}

impl TestServer {
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

    /// Create `GET` request
    pub fn get<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.get(self.url(path.as_ref()).as_str())
    }

    /// Create https `GET` request
    pub fn sget<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.get(self.surl(path.as_ref()).as_str())
    }

    /// Create `POST` request
    pub fn post<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.post(self.url(path.as_ref()).as_str())
    }

    /// Create https `POST` request
    pub fn spost<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.post(self.surl(path.as_ref()).as_str())
    }

    /// Create `HEAD` request
    pub fn head<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.head(self.url(path.as_ref()).as_str())
    }

    /// Create https `HEAD` request
    pub fn shead<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.head(self.surl(path.as_ref()).as_str())
    }

    /// Create `PUT` request
    pub fn put<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.put(self.url(path.as_ref()).as_str())
    }

    /// Create https `PUT` request
    pub fn sput<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.put(self.surl(path.as_ref()).as_str())
    }

    /// Create `PATCH` request
    pub fn patch<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.patch(self.url(path.as_ref()).as_str())
    }

    /// Create https `PATCH` request
    pub fn spatch<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.patch(self.surl(path.as_ref()).as_str())
    }

    /// Create `DELETE` request
    pub fn delete<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.delete(self.url(path.as_ref()).as_str())
    }

    /// Create https `DELETE` request
    pub fn sdelete<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.delete(self.surl(path.as_ref()).as_str())
    }

    /// Create `OPTIONS` request
    pub fn options<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.options(self.url(path.as_ref()).as_str())
    }

    /// Create https `OPTIONS` request
    pub fn soptions<S: AsRef<str>>(&self, path: S) -> ClientRequest {
        self.client.options(self.surl(path.as_ref()).as_str())
    }

    /// Connect to test http server
    pub fn request<S: AsRef<str>>(&self, method: Method, path: S) -> ClientRequest {
        self.client.request(method, path.as_ref())
    }

    pub async fn load_body<S>(
        &mut self,
        mut response: ClientResponse<S>,
    ) -> Result<Bytes, PayloadError>
    where
        S: Stream<Item = Result<Bytes, PayloadError>> + Unpin + 'static,
    {
        response.body().limit(10_485_760).await
    }

    /// Connect to websocket server at a given path
    pub async fn ws_at(
        &mut self,
        path: &str,
    ) -> Result<Framed<impl AsyncRead + AsyncWrite, crate::ws::Codec>, WsClientError>
    {
        let url = self.url(path);
        let connect = self.client.ws(url).connect();
        connect.await.map(|(_, framed)| framed)
    }

    /// Connect to a websocket server
    pub async fn ws(
        &mut self,
    ) -> Result<Framed<impl AsyncRead + AsyncWrite, crate::ws::Codec>, WsClientError>
    {
        self.ws_at("/").await
    }

    /// Stop http server
    fn stop(&mut self) {
        self.system.stop();
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop()
    }
}
