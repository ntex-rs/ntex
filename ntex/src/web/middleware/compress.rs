//! `Middleware` for compressing response body.
use std::task::{Context, Poll};
use std::{cmp, future::Future, marker, pin::Pin, str::FromStr};

use crate::http::encoding::Encoder;
use crate::http::header::{ContentEncoding, ACCEPT_ENCODING};
use crate::service::{Ctx, Middleware, Service, ServiceCall};
use crate::web::{BodyEncoding, ErrorRenderer, WebRequest, WebResponse};

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
pub struct Compress {
    enc: ContentEncoding,
}

impl Compress {
    /// Create new `Compress` middleware with default encoding.
    pub fn new(encoding: ContentEncoding) -> Self {
        Compress { enc: encoding }
    }
}

impl Default for Compress {
    fn default() -> Self {
        Compress::new(ContentEncoding::Auto)
    }
}

impl<S> Middleware<S> for Compress {
    type Service = CompressMiddleware<S>;

    fn create(&self, service: S) -> Self::Service {
        CompressMiddleware {
            service,
            encoding: self.enc,
        }
    }
}

pub struct CompressMiddleware<S> {
    service: S,
    encoding: ContentEncoding,
}

impl<S, E> Service<WebRequest<E>> for CompressMiddleware<S>
where
    S: Service<WebRequest<E>, Response = WebResponse>,
    E: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = S::Error;
    type Future<'f> = CompressResponse<'f, S, E> where S: 'f;

    crate::forward_poll_ready!(service);
    crate::forward_poll_shutdown!(service);

    fn call<'a>(&'a self, req: WebRequest<E>, ctx: Ctx<'a, Self>) -> Self::Future<'a> {
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
            fut: ctx.call(&self.service, req),
            _t: marker::PhantomData,
        }
    }
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    pub struct CompressResponse<'f, S: Service<WebRequest<E>>, E>
    where S: 'f, E: 'f
    {
        #[pin]
        fut: ServiceCall<'f, S, WebRequest<E>>,
        encoding: ContentEncoding,
        _t: marker::PhantomData<E>,
    }
}

impl<'f, S, E> Future for CompressResponse<'f, S, E>
where
    S: Service<WebRequest<E>, Response = WebResponse>,
    E: ErrorRenderer,
{
    type Output = Result<WebResponse, S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match this.fut.poll(cx)? {
            Poll::Ready(resp) => {
                let enc = if let Some(enc) = resp.response().get_encoding() {
                    enc
                } else {
                    *this.encoding
                };

                Poll::Ready(Ok(
                    resp.map_body(move |head, body| Encoder::response(enc, head, body))
                ))
            }
            Poll::Pending => Poll::Pending,
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
            _ => f64::from_str(parts[1]).unwrap_or(0.0),
        };
        Some(AcceptEncoding { encoding, quality })
    }

    /// Parse a raw Accept-Encoding header value into an ordered list.
    fn parse(raw: &str, encoding: ContentEncoding) -> ContentEncoding {
        let mut encodings: Vec<_> = raw
            .replace(' ', "")
            .split(',')
            .map(AcceptEncoding::new)
            .collect();
        encodings.sort();

        for enc in encodings.into_iter().flatten() {
            if encoding == ContentEncoding::Auto {
                return enc.encoding;
            } else if encoding == enc.encoding {
                return encoding;
            }
        }
        ContentEncoding::Identity
    }
}
