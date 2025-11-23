//! `Middleware` for compressing response body.
use std::{cmp, str::FromStr};

use crate::http::encoding::Encoder;
use crate::http::header::{ACCEPT_ENCODING, ContentEncoding};
use crate::service::{Middleware, Service, ServiceCtx};
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

#[derive(Debug)]
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

    crate::forward_poll!(service);
    crate::forward_ready!(service);
    crate::forward_shutdown!(service);

    async fn call(
        &self,
        req: WebRequest<E>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<WebResponse, S::Error> {
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

        let resp = ctx.call(&self.service, req).await?;

        let enc = if let Some(enc) = resp.response().get_encoding() {
            enc
        } else {
            encoding
        };

        Ok(resp.map_body(move |head, body| Encoder::response(enc, head, body)))
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

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;

    #[test]
    fn test_accepting_encodings_equal() {
        let accepting_encoding = AcceptEncoding {
            encoding: ContentEncoding::Auto,
            quality: 0.0,
        };
        let accepting_encoding2 = AcceptEncoding {
            encoding: ContentEncoding::Br,
            quality: 0.0,
        };

        assert!(accepting_encoding == accepting_encoding2);
    }

    #[test]
    fn test_accepting_encodings_not_equal() {
        let accepting_encoding = AcceptEncoding {
            encoding: ContentEncoding::Auto,
            quality: 1.0,
        };
        let accepting_encoding2 = AcceptEncoding {
            encoding: ContentEncoding::Br,
            quality: 0.0,
        };

        assert!(accepting_encoding != accepting_encoding2);
    }

    #[test]
    fn test_accepting_encodings_cmp_order_less() {
        let accepting_encoding = AcceptEncoding {
            encoding: ContentEncoding::Auto,
            quality: 1.0,
        };
        let accepting_encoding2 = AcceptEncoding {
            encoding: ContentEncoding::Br,
            quality: 0.0,
        };

        assert_eq!(accepting_encoding.cmp(&accepting_encoding2), Ordering::Less);
    }

    #[test]
    fn test_accepting_encodings_cmp_order_equal() {
        let accepting_encoding = AcceptEncoding {
            encoding: ContentEncoding::Auto,
            quality: 1.0,
        };

        assert_eq!(accepting_encoding.cmp(&accepting_encoding), Ordering::Equal);
    }

    #[test]
    fn test_accepting_encoding_from_tag_with_valid_quality() {
        let accepting_encoding = AcceptEncoding::new("gzip;0.8").unwrap();
        assert_eq!(accepting_encoding.quality, 0.8);
    }

    #[test]
    fn test_accepting_encoding_from_tag_with_invalid_quality() {
        let accepting_encoding = AcceptEncoding::new("gzip;q=abc").unwrap();
        assert_eq!(accepting_encoding.quality, 0.0);
    }
}
