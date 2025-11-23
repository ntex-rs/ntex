//! Request logging middleware
use std::task::{Context, Poll};
use std::{env, error::Error, fmt, fmt::Display, rc::Rc, time};

use regex::Regex;

use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::header::HeaderName;
use crate::service::{Middleware, Service, ServiceCtx};
use crate::util::{Bytes, HashSet};
use crate::web::{HttpResponse, WebRequest, WebResponse};

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
#[derive(Debug)]
pub struct Logger {
    inner: Rc<Inner>,
}

#[derive(Debug)]
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

impl<S> Middleware<S> for Logger {
    type Service = LoggerMiddleware<S>;

    fn create(&self, service: S) -> Self::Service {
        LoggerMiddleware {
            service,
            inner: self.inner.clone(),
        }
    }
}

#[derive(Debug)]
/// Logger middleware
pub struct LoggerMiddleware<S> {
    inner: Rc<Inner>,
    service: S,
}

impl<S, E> Service<WebRequest<E>> for LoggerMiddleware<S>
where
    S: Service<WebRequest<E>, Response = WebResponse>,
{
    type Response = WebResponse;
    type Error = S::Error;

    crate::forward_poll!(service);
    crate::forward_ready!(service);
    crate::forward_shutdown!(service);

    async fn call(
        &self,
        req: WebRequest<E>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        if self.inner.exclude.contains(req.path()) {
            ctx.call(&self.service, req).await
        } else {
            let time = time::SystemTime::now();
            let mut format = self.inner.format.clone();

            for unit in &mut format.0 {
                unit.render_request(time, &req);
            }

            let res = ctx.call(&self.service, req).await?;
            for unit in &mut format.0 {
                unit.render_response(res.response());
            }

            Ok(res.map_body(move |_, body| {
                ResponseBody::Other(Body::from_message(StreamLog {
                    body,
                    time,
                    format: Some(format),
                    size: 0,
                }))
            }))
        }
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
    ) -> Poll<Option<Result<Bytes, Rc<dyn Error>>>> {
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
#[derive(Clone, Debug)]
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
        log::trace!("Access log format: {s}");
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
                fmt.write_fmt(format_args!("{rt:.6}"))
            }
            FormatText::TimeMillis => {
                let rt = entry_time.elapsed().unwrap();
                let rt = (rt.as_nanos() as f64) / 1_000_000.0;
                fmt.write_fmt(format_args!("{rt:.6}"))
            }
            FormatText::EnvironHeader(ref name) => {
                if let Ok(val) = env::var(name) {
                    fmt.write_fmt(format_args!("{val}"))
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
                    val.to_str().unwrap_or("-")
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
                let q = req.query_string();
                *self = if q.is_empty() {
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
                        q,
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
                    val.to_str().unwrap_or("-")
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

impl fmt::Display for FormatDisplay<'_> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        (self.0)(fmt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{StatusCode, header};
    use crate::service::{IntoService, Pipeline};
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

        let srv = Pipeline::new(Middleware::create(&logger, srv.into_service())).bind();
        assert!(lazy(|cx| srv.poll_ready(cx).is_ready()).await);
        assert!(lazy(|cx| srv.poll_shutdown(cx).is_ready()).await);

        let req = TestRequest::with_header(
            header::USER_AGENT,
            header::HeaderValue::from_static("NTEX"),
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
    async fn test_request_line() {
        let mut format = Format::new("%r");
        let req = TestRequest::with_header(
            header::USER_AGENT,
            header::HeaderValue::from_static("NTEX"),
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
        assert_eq!(s, "GET /test/route/yeah?q=test HTTP/1.1");
    }

    #[crate::rt_test]
    async fn test_url_path() {
        let mut format = Format::new("%T %U");
        let req = TestRequest::with_header(
            header::USER_AGENT,
            header::HeaderValue::from_static("NTEX"),
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
            header::HeaderValue::from_static("NTEX"),
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
        assert!(s.contains("NTEX"));
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
