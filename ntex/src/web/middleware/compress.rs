//! `Middleware` for compressing response body.
use std::cmp;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};

use futures::future::{ok, Ready};
use pin_project::pin_project;

use crate::http::encoding::Encoder;
use crate::http::header::{ContentEncoding, ACCEPT_ENCODING};
use crate::service::{Service, Transform};

use crate::web::dev::{WebRequest, WebResponse};
use crate::web::{BodyEncoding, ErrorRenderer};

#[derive(Debug, Clone)]
/// `Middleware` for compressing response body.
///
/// Use `BodyEncoding` trait for overriding response compression.
/// To disable compression set encoding to `ContentEncoding::Identity` value.
///
/// ```rust
/// use ntex::web::{self, middleware, App, HttpResponse};
///
/// fn main() {
///     let app = App::new()
///         .wrap(middleware::Compress::default())
///         .service(
///             web::resource("/test")
///                 .route(web::get().to(|| async { HttpResponse::Ok() }))
///                 .route(web::head().to(|| async { HttpResponse::MethodNotAllowed() }))
///         );
/// }
/// ```
pub struct Compress<Err> {
    enc: ContentEncoding,
    _t: PhantomData<Err>,
}

impl<Err> Compress<Err> {
    /// Create new `Compress` middleware with default encoding.
    pub fn new(encoding: ContentEncoding) -> Self {
        Compress {
            enc: encoding,
            _t: PhantomData,
        }
    }
}

impl<Err> Default for Compress<Err> {
    fn default() -> Self {
        Compress::new(ContentEncoding::Auto)
    }
}

impl<S, E> Transform<S> for Compress<E>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse>,
    E: ErrorRenderer,
{
    type Request = WebRequest<E>;
    type Response = WebResponse;
    type Error = S::Error;
    type InitError = ();
    type Transform = CompressMiddleware<S, E>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(CompressMiddleware {
            service,
            encoding: self.enc,
            _t: PhantomData,
        })
    }
}

pub struct CompressMiddleware<S, E> {
    service: S,
    encoding: ContentEncoding,
    _t: PhantomData<E>,
}

impl<S, E> Service for CompressMiddleware<S, E>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse>,
    E: ErrorRenderer,
{
    type Request = WebRequest<E>;
    type Response = WebResponse;
    type Error = S::Error;
    type Future = CompressResponse<S, E>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    fn call(&self, req: WebRequest<E>) -> Self::Future {
        // negotiate content-encoding
        let encoding = if let Some(val) = req.headers().get(&ACCEPT_ENCODING) {
            if let Ok(enc) = val.to_str() {
                AcceptEncoding::parse(enc, self.encoding)
            } else {
                ContentEncoding::Identity
            }
        } else {
            ContentEncoding::Identity
        };

        CompressResponse {
            encoding,
            fut: self.service.call(req),
            _t: PhantomData,
        }
    }
}

#[doc(hidden)]
#[pin_project]
pub struct CompressResponse<S, E>
where
    S: Service,
{
    #[pin]
    fut: S::Future,
    encoding: ContentEncoding,
    _t: PhantomData<E>,
}

impl<S, E> Future for CompressResponse<S, E>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse>,
    E: ErrorRenderer,
{
    type Output = Result<WebResponse, S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match futures::ready!(this.fut.poll(cx)) {
            Ok(resp) => {
                let enc = if let Some(enc) = resp.response().get_encoding() {
                    enc
                } else {
                    *this.encoding
                };

                Poll::Ready(Ok(
                    resp.map_body(move |head, body| Encoder::response(enc, head, body))
                ))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

struct AcceptEncoding {
    encoding: ContentEncoding,
    quality: f64,
}

impl Eq for AcceptEncoding {}

impl Ord for AcceptEncoding {
    #[allow(clippy::comparison_chain)]
    fn cmp(&self, other: &AcceptEncoding) -> cmp::Ordering {
        if self.quality > other.quality {
            cmp::Ordering::Less
        } else if self.quality < other.quality {
            cmp::Ordering::Greater
        } else {
            cmp::Ordering::Equal
        }
    }
}

impl PartialOrd for AcceptEncoding {
    fn partial_cmp(&self, other: &AcceptEncoding) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for AcceptEncoding {
    fn eq(&self, other: &AcceptEncoding) -> bool {
        self.quality == other.quality
    }
}

impl AcceptEncoding {
    fn new(tag: &str) -> Option<AcceptEncoding> {
        let parts: Vec<&str> = tag.split(';').collect();
        let encoding = match parts.len() {
            0 => return None,
            _ => ContentEncoding::from(parts[0]),
        };
        let quality = match parts.len() {
            1 => encoding.quality(),
            _ => match f64::from_str(parts[1]) {
                Ok(q) => q,
                Err(_) => 0.0,
            },
        };
        Some(AcceptEncoding { encoding, quality })
    }

    /// Parse a raw Accept-Encoding header value into an ordered list.
    fn parse(raw: &str, encoding: ContentEncoding) -> ContentEncoding {
        let mut encodings: Vec<_> = raw
            .replace(' ', "")
            .split(',')
            .map(|l| AcceptEncoding::new(l))
            .collect();
        encodings.sort();

        for enc in encodings {
            if let Some(enc) = enc {
                if encoding == ContentEncoding::Auto {
                    return enc.encoding;
                } else if encoding == enc.encoding {
                    return encoding;
                }
            }
        }
        ContentEncoding::Identity
    }
}
