//! Request logging middleware
use std::fmt::{self, Display};
use std::task::{Context, Poll};
use std::{convert::TryFrom, env, error::Error, future::Future, pin::Pin, rc::Rc, time};

use bytes::Bytes;
use regex::Regex;

use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::header::HeaderName;
use crate::service::{Service, Transform};
use crate::util::{Either, HashSet, Ready};
use crate::web::dev::{WebRequest, WebResponse};
use crate::web::HttpResponse;

/// `Middleware` for logging request and response info to the terminal.
///
/// `Logger` middleware uses standard log crate to log information. You should
/// enable logger for `ntex` package to see access log.
/// ([`env_logger`](https://docs.rs/env_logger/*/env_logger/) or similar)
///
/// ## Usage
///
/// Create `Logger` middleware with the specified `format`.
/// Default `Logger` could be created with `default` method, it uses the
/// default format:
///
/// ```ignore
///  %a "%r" %s %b "%{Referer}i" "%{User-Agent}i" %T
/// ```
/// ```rust
/// use ntex::web::App;
/// use ntex::web::middleware::Logger;
///
/// fn main() {
///     std::env::set_var("RUST_LOG", "ntex=info");
///     env_logger::init();
///
///     let app = App::new()
///         .wrap(Logger::default())
///         .wrap(Logger::new("%a %{User-Agent}i"));
/// }
/// ```
///
/// ## Format
///
/// `%%`  The percent sign
///
/// `%a`  Remote IP-address (IP-address of proxy if using reverse proxy)
///
/// `%t`  Time when the request was started to process (in rfc3339 format)
///
/// `%r`  First line of request
///
/// `%s`  Response status code
///
/// `%b`  Size of response in bytes, including HTTP headers
///
/// `%T` Time taken to serve the request, in seconds with floating fraction in
/// .06f format
///
/// `%D`  Time taken to serve the request, in milliseconds
///
/// `%U`  Request URL
///
/// `%{FOO}i`  request.headers['FOO']
///
/// `%{FOO}o`  response.headers['FOO']
///
/// `%{FOO}e`  os.environ['FOO']
///
pub struct Logger {
    inner: Rc<Inner>,
}

struct Inner {
    format: Format,
    exclude: HashSet<String>,
}

impl Logger {
    /// Create `Logger` middleware with the specified `format`.
    pub fn new(format: &str) -> Logger {
        Logger {
            inner: Rc::new(Inner {
                format: Format::new(format),
                exclude: HashSet::default(),
            }),
        }
    }

    /// Ignore and do not log access info for specified path.
    pub fn exclude<T: Into<String>>(mut self, path: T) -> Self {
        Rc::get_mut(&mut self.inner)
            .unwrap()
            .exclude
            .insert(path.into());
        self
    }
}

impl Default for Logger {
    /// Create `Logger` middleware with format:
    ///
    /// ```ignore
    /// %a "%r" %s %b "%{Referer}i" "%{User-Agent}i" %T
    /// ```
    fn default() -> Self {
        Logger {
            inner: Rc::new(Inner {
                format: Format::default(),
                exclude: HashSet::default(),
            }),
        }
    }
}

impl<S, Err> Transform<S> for Logger
where
    S: Service<Request = WebRequest<Err>, Response = WebResponse>,
{
    type Request = WebRequest<Err>;
    type Response = WebResponse;
    type Error = S::Error;
    type InitError = ();
    type Transform = LoggerMiddleware<S>;
    type Future = Ready<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        Ready::Ok(LoggerMiddleware {
            service,
            inner: self.inner.clone(),
        })
    }
}

/// Logger middleware
pub struct LoggerMiddleware<S> {
    inner: Rc<Inner>,
    service: S,
}

impl<S, E> Service for LoggerMiddleware<S>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse>,
{
    type Request = WebRequest<E>;
    type Response = WebResponse;
    type Error = S::Error;
    type Future = Either<LoggerResponse<S>, S::Future>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: WebRequest<E>) -> Self::Future {
        if self.inner.exclude.contains(req.path()) {
            Either::Right(self.service.call(req))
        } else {
            let time = time::SystemTime::now();
            let mut format = self.inner.format.clone();

            for unit in &mut format.0 {
                unit.render_request(time, &req);
            }
            Either::Left(LoggerResponse {
                time,
                format: Some(format),
                fut: self.service.call(req),
            })
        }
    }
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    pub struct LoggerResponse<S: Service>
    {
        #[pin]
        fut: S::Future,
        time: time::SystemTime,
        format: Option<Format>,
    }
}

impl<S, E> Future for LoggerResponse<S>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse>,
{
    type Output = Result<WebResponse, S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let res = match this.fut.poll(cx) {
            Poll::Ready(Ok(res)) => res,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        };

        if let Some(ref mut format) = this.format {
            for unit in &mut format.0 {
                unit.render_response(res.response());
            }
        }

        let time = *this.time;
        let format = this.format.take();

        Poll::Ready(Ok(res.map_body(move |_, body| {
            ResponseBody::Other(Body::from_message(StreamLog {
                body,
                time,
                format,
                size: 0,
            }))
        })))
    }
}

struct StreamLog {
    body: ResponseBody<Body>,
    format: Option<Format>,
    size: usize,
    time: time::SystemTime,
}

impl Drop for StreamLog {
    fn drop(&mut self) {
        if let Some(ref format) = self.format {
            let render = |fmt: &mut fmt::Formatter<'_>| {
                for unit in &format.0 {
                    unit.render(fmt, self.size, self.time)?;
                }
                Ok(())
            };
            log::info!("{}", FormatDisplay(&render));
        }
    }
}

impl MessageBody for StreamLog {
    fn size(&self) -> BodySize {
        self.body.size()
    }

    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        match self.body.poll_next_chunk(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.size += chunk.len();
                Poll::Ready(Some(Ok(chunk)))
            }
            val => val,
        }
    }
}

/// A formatting style for the `Logger`, consisting of multiple
/// `FormatText`s concatenated into one line.
#[derive(Clone)]
#[doc(hidden)]
struct Format(Vec<FormatText>);

impl Default for Format {
    /// Return the default formatting style for the `Logger`:
    fn default() -> Format {
        Format::new(r#"%a "%r" %s %b "%{Referer}i" "%{User-Agent}i" %T"#)
    }
}

impl Format {
    /// Create a `Format` from a format string.
    ///
    /// Returns `None` if the format string syntax is incorrect.
    fn new(s: &str) -> Format {
        log::trace!("Access log format: {}", s);
        let fmt = Regex::new(r"%(\{([A-Za-z0-9\-_]+)\}([ioe])|[atPrUsbTD]?)").unwrap();

        let mut idx = 0;
        let mut results = Vec::new();
        for cap in fmt.captures_iter(s) {
            let m = cap.get(0).unwrap();
            let pos = m.start();
            if idx != pos {
                results.push(FormatText::Str(s[idx..pos].to_owned()));
            }
            idx = m.end();

            if let Some(key) = cap.get(2) {
                results.push(match cap.get(3).unwrap().as_str() {
                    "i" => FormatText::RequestHeader(
                        HeaderName::try_from(key.as_str()).unwrap(),
                    ),
                    "o" => FormatText::ResponseHeader(
                        HeaderName::try_from(key.as_str()).unwrap(),
                    ),
                    "e" => FormatText::EnvironHeader(key.as_str().to_owned()),
                    _ => unreachable!(),
                })
            } else {
                let m = cap.get(1).unwrap();
                results.push(match m.as_str() {
                    "%" => FormatText::Percent,
                    "a" => FormatText::RemoteAddr,
                    "t" => FormatText::RequestTime,
                    "r" => FormatText::RequestLine,
                    "s" => FormatText::ResponseStatus,
                    "b" => FormatText::ResponseSize,
                    "U" => FormatText::UrlPath,
                    "T" => FormatText::Time,
                    "D" => FormatText::TimeMillis,
                    _ => FormatText::Str(m.as_str().to_owned()),
                });
            }
        }
        if idx != s.len() {
            results.push(FormatText::Str(s[idx..].to_owned()));
        }

        Format(results)
    }
}

/// A string of text to be logged. This is either one of the data
/// fields supported by the `Logger`, or a custom `String`.
#[doc(hidden)]
#[derive(Debug, Clone)]
enum FormatText {
    Str(String),
    Percent,
    RequestLine,
    RequestTime,
    ResponseStatus,
    ResponseSize,
    Time,
    TimeMillis,
    RemoteAddr,
    UrlPath,
    RequestHeader(HeaderName),
    ResponseHeader(HeaderName),
    EnvironHeader(String),
}

impl FormatText {
    fn render(
        &self,
        fmt: &mut fmt::Formatter<'_>,
        size: usize,
        entry_time: time::SystemTime,
    ) -> Result<(), fmt::Error> {
        match *self {
            FormatText::Str(ref string) => fmt.write_str(string),
            FormatText::Percent => "%".fmt(fmt),
            FormatText::ResponseSize => size.fmt(fmt),
            FormatText::Time => {
                let rt = entry_time.elapsed().unwrap();
                let rt = rt.as_secs_f64();
                fmt.write_fmt(format_args!("{:.6}", rt))
            }
            FormatText::TimeMillis => {
                let rt = entry_time.elapsed().unwrap();
                let rt = (rt.as_nanos() as f64) / 1_000_000.0;
                fmt.write_fmt(format_args!("{:.6}", rt))
            }
            FormatText::EnvironHeader(ref name) => {
                if let Ok(val) = env::var(name) {
                    fmt.write_fmt(format_args!("{}", val))
                } else {
                    "-".fmt(fmt)
                }
            }
            _ => Ok(()),
        }
    }

    fn render_response<B>(&mut self, res: &HttpResponse<B>) {
        match *self {
            FormatText::ResponseStatus => {
                *self = FormatText::Str(format!("{}", res.status().as_u16()))
            }
            FormatText::ResponseHeader(ref name) => {
                let s = if let Some(val) = res.headers().get(name) {
                    if let Ok(s) = val.to_str() {
                        s
                    } else {
                        "-"
                    }
                } else {
                    "-"
                };
                *self = FormatText::Str(s.to_string())
            }
            _ => (),
        }
    }

    fn render_request<E>(&mut self, now: time::SystemTime, req: &WebRequest<E>) {
        match *self {
            FormatText::RequestLine => {
                *self = if req.query_string().is_empty() {
                    FormatText::Str(format!(
                        "{} {} {:?}",
                        req.method(),
                        req.path(),
                        req.version()
                    ))
                } else {
                    FormatText::Str(format!(
                        "{} {}?{} {:?}",
                        req.method(),
                        req.path(),
                        req.query_string(),
                        req.version()
                    ))
                };
            }
            FormatText::UrlPath => *self = FormatText::Str(req.path().to_string()),
            FormatText::RequestTime => {
                *self = FormatText::Str(httpdate::HttpDate::from(now).to_string())
            }
            FormatText::RequestHeader(ref name) => {
                let s = if let Some(val) = req.headers().get(name) {
                    if let Ok(s) = val.to_str() {
                        s
                    } else {
                        "-"
                    }
                } else {
                    "-"
                };
                *self = FormatText::Str(s.to_string());
            }
            FormatText::RemoteAddr => {
                let s = if let Some(remote) = req.connection_info().remote() {
                    FormatText::Str(remote.to_string())
                } else {
                    FormatText::Str("-".to_string())
                };
                *self = s;
            }
            _ => (),
        }
    }
}

pub(crate) struct FormatDisplay<'a>(
    &'a dyn Fn(&mut fmt::Formatter<'_>) -> Result<(), fmt::Error>,
);

impl<'a> fmt::Display for FormatDisplay<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        (self.0)(fmt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{header, StatusCode};
    use crate::service::{IntoService, Service, Transform};
    use crate::util::lazy;
    use crate::web::test::{self, TestRequest};
    use crate::web::{DefaultError, Error};

    #[crate::rt_test]
    async fn test_logger() {
        let srv = |req: WebRequest<DefaultError>| async move {
            Ok::<_, Error>(
                req.into_response(
                    HttpResponse::build(StatusCode::OK)
                        .header("X-Test", "ttt")
                        .body("TEST"),
                ),
            )
        };
        let _logger = Logger::default();
        let logger = Logger::new("%% %{User-Agent}i %{X-Test}o %{HOME}e %D %% test")
            .exclude("/test");

        let srv = Transform::new_transform(&logger, srv.into_service())
            .await
            .unwrap();

        assert!(lazy(|cx| srv.poll_ready(cx).is_ready()).await);
        assert!(lazy(|cx| srv.poll_shutdown(cx, true).is_ready()).await);

        let req = TestRequest::with_header(
            header::USER_AGENT,
            header::HeaderValue::from_static("NTEX-WEB"),
        )
        .to_srv_request();
        let res = srv.call(req).await.unwrap();
        let body = test::read_body(res).await;
        assert_eq!(body, Bytes::from_static(b"TEST"));
        assert_eq!(body.size(), BodySize::Sized(4));
        drop(body);

        let req = TestRequest::with_uri("/test").to_srv_request();
        let res = srv.call(req).await.unwrap();
        let body = test::read_body(res).await;
        assert_eq!(body, Bytes::from_static(b"TEST"));
    }

    #[crate::rt_test]
    async fn test_url_path() {
        let mut format = Format::new("%T %U");
        let req = TestRequest::with_header(
            header::USER_AGENT,
            header::HeaderValue::from_static("ACTIX-WEB"),
        )
        .uri("/test/route/yeah?q=test")
        .to_srv_request();

        let now = time::SystemTime::now();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let render = |fmt: &mut fmt::Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, now)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        assert!(s.contains("/test/route/yeah"));
    }

    #[crate::rt_test]
    async fn test_default_format() {
        let mut format = Format::default();

        let req = TestRequest::with_header(
            header::USER_AGENT,
            header::HeaderValue::from_static("ACTIX-WEB"),
        )
        .to_srv_request();

        let now = time::SystemTime::now();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let entry_time = time::SystemTime::now();
        let render = |fmt: &mut fmt::Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, entry_time)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        assert!(s.contains("GET / HTTP/1.1"));
        assert!(s.contains("200 1024"));
        assert!(s.contains("ACTIX-WEB"));
    }

    #[crate::rt_test]
    async fn test_request_time_format() {
        let mut format = Format::new("%t");
        let req = TestRequest::default().to_srv_request();

        let now = time::SystemTime::now();
        for unit in &mut format.0 {
            unit.render_request(now, &req);
        }

        let resp = HttpResponse::build(StatusCode::OK).force_close().finish();
        for unit in &mut format.0 {
            unit.render_response(&resp);
        }

        let render = |fmt: &mut fmt::Formatter<'_>| {
            for unit in &format.0 {
                unit.render(fmt, 1024, now)?;
            }
            Ok(())
        };
        let s = format!("{}", FormatDisplay(&render));
        assert!(s.contains(&httpdate::HttpDate::from(now).to_string()));
    }
}
