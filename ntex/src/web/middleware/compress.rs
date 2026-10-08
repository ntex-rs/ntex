//! `Middleware` for compressing response body.
use crate::http::encoding::Encoder;
use crate::http::header::{ACCEPT_ENCODING, ContentEncoding};
use crate::service::{Ctx, Middleware, Service};
use crate::web::{BodyEncoding, State, WebRequest, WebResponse};

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
///     let app = App::default()
///         .middleware(middleware::Compress::default())
///         .service(
///             web::resource("/test")
///                 .route(web::get().to(async || { HttpResponse::Ok() }))
///                 .route(web::head().to(async || { HttpResponse::MethodNotAllowed() }))
///         );
/// }
/// ```
pub struct Compress {
    enc: ContentEncoding,
}

impl Compress {
    /// Create new `Compress` middleware with the specified encoding.
    ///
    /// Use `Compress::default()` to select the encoding automatically
    /// ([`ContentEncoding::Auto`]).
    pub fn new(encoding: ContentEncoding) -> Self {
        Compress { enc: encoding }
    }
}

impl Default for Compress {
    fn default() -> Self {
        Compress::new(ContentEncoding::Auto)
    }
}

impl<S, St> Middleware<S, St> for Compress {
    type Service = CompressMiddleware<S>;

    fn create(&self, _: &St, service: S) -> Self::Service {
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

impl<S, St, In> Service<St, WebRequest<In>> for CompressMiddleware<S>
where
    S: Service<St, WebRequest<In>, Res = WebResponse>,
    St: State,
{
    type Res = WebResponse;
    type Error = S::Error;

    crate::forward_ready!(St, service);
    crate::forward_shutdown!(St, service);

    async fn call(
        &self,
        req: WebRequest<In>,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<WebResponse, S::Error> {
        // negotiate content-encoding
        let values = req
            .headers()
            .get_all(&ACCEPT_ENCODING)
            .filter_map(|val| val.to_str().ok());
        let encoding = AcceptEncoding::parse(values, self.encoding);

        let resp = ctx.call(&self.service, req).await?;

        let enc = if let Some(enc) = resp.response().get_encoding() {
            enc
        } else {
            encoding
        };

        Ok(resp.map_body(move |head, body| Encoder::response(enc, head, body)))
    }
}

/// Encodings that can be picked for a `*` entry.
const WILDCARD: [ContentEncoding; 3] = [
    ContentEncoding::Zstd,
    ContentEncoding::Gzip,
    ContentEncoding::Deflate,
];

#[derive(Debug, PartialEq)]
struct AcceptEncoding {
    /// `None` for the `*` entry
    encoding: Option<ContentEncoding>,
    /// Client weight in thousandths, `0..=1000`
    q: u16,
}

impl AcceptEncoding {
    fn new(tag: &str) -> Option<AcceptEncoding> {
        let mut parts = tag.split(';');
        let name = parts.next()?.trim();
        if name.is_empty() {
            return None;
        }
        let encoding = if name == "*" {
            None
        } else {
            Some(ContentEncoding::from(name))
        };

        // a malformed weight makes the entry unacceptable
        let q = parts
            .filter_map(|param| param.split_once('='))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
            .map_or(1000, |(_, val)| parse_q(val.trim()).unwrap_or(0));
        Some(AcceptEncoding { encoding, q })
    }

    /// Pick a response encoding from the `Accept-Encoding` header values.
    ///
    /// Entries are ranked by the client's `q` value, ties go to the
    /// server's preference. `q=0` means "not acceptable" and `*` covers
    /// every encoding the client did not list.
    fn parse<'a, I>(values: I, encoding: ContentEncoding) -> ContentEncoding
    where
        I: Iterator<Item = &'a str>,
    {
        let mut listed: Vec<(ContentEncoding, u16)> = Vec::new();
        let mut wildcard = None;
        for item in values.flat_map(|val| val.split(',')) {
            match AcceptEncoding::new(item) {
                Some(AcceptEncoding {
                    encoding: Some(enc),
                    q,
                }) => listed.push((enc, q)),
                Some(AcceptEncoding { encoding: None, q }) => {
                    wildcard.get_or_insert(q);
                }
                None => {}
            }
        }
        let weight = |enc: ContentEncoding| {
            listed
                .iter()
                .find(|(e, _)| *e == enc)
                .map(|(_, q)| *q)
                .or(wildcard)
                .unwrap_or(0)
        };

        if encoding != ContentEncoding::Auto {
            return if weight(encoding) > 0 {
                encoding
            } else {
                ContentEncoding::Identity
            };
        }

        let mut best: Option<(ContentEncoding, u16)> = None;
        let candidates = listed.iter().map(|(enc, _)| *enc).chain(WILDCARD);
        for enc in candidates.filter(|enc| Encoder::can_encode(*enc)) {
            let q = weight(enc);
            let better = match best {
                None => q > 0,
                Some((b, bq)) => q > bq || (q == bq && enc.quality() > b.quality()),
            };
            if better {
                best = Some((enc, q));
            }
        }
        best.map_or(ContentEncoding::Identity, |(enc, _)| enc)
    }
}

/// Parse an RFC 9110 `qvalue` into thousandths.
fn parse_q(val: &str) -> Option<u16> {
    let (int, frac) = match val.split_once('.') {
        Some((int, frac)) => (int, frac),
        None => (val, ""),
    };
    if frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let int = match int {
        "0" => 0,
        "1" => 1000,
        _ => return None,
    };
    let frac = frac
        .bytes()
        .zip([100, 10, 1])
        .map(|(b, scale)| u16::from(b - b'0') * scale)
        .sum::<u16>();
    if int + frac > 1000 { None } else { Some(int + frac) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str, encoding: ContentEncoding) -> ContentEncoding {
        AcceptEncoding::parse([raw].into_iter(), encoding)
    }

    #[test]
    fn test_parse_q() {
        for (val, q) in [
            ("0", 0),
            ("0.", 0),
            ("0.5", 500),
            ("0.05", 50),
            ("0.125", 125),
            ("0.999", 999),
            ("1", 1000),
            ("1.", 1000),
            ("1.000", 1000),
        ] {
            assert_eq!(parse_q(val), Some(q), "{val}");
        }
        for val in [
            "", ".5", "0.1234", "1.001", "1.5", "2", "-0", "0.a", "0.00a", "01", "abc",
        ] {
            assert_eq!(parse_q(val), None, "{val}");
        }
    }

    #[test]
    fn test_accept_encoding_entry() {
        let entry = |tag| AcceptEncoding::new(tag).unwrap();
        assert_eq!(
            entry("gzip"),
            AcceptEncoding {
                encoding: Some(ContentEncoding::Gzip),
                q: 1000
            }
        );
        assert_eq!(entry(" gzip ; q=0.5 ").q, 500);
        assert_eq!(entry("gzip;Q=0.5").q, 500);
        assert_eq!(entry("gzip;level=1;q=0.25").q, 250);
        assert_eq!(entry("gzip;q=abc").q, 0);
        assert_eq!(entry("gzip;0.8").q, 1000);
        assert_eq!(entry("*;q=0.1").encoding, None);
        assert!(AcceptEncoding::new(" ").is_none());
    }

    #[test]
    fn test_auto_skips_unsupported_encodings() {
        let auto = ContentEncoding::Auto;
        assert_eq!(
            parse("gzip, deflate, br, zstd", auto),
            ContentEncoding::Zstd
        );
        assert_eq!(parse("gzip, deflate, br", auto), ContentEncoding::Gzip);
        assert_eq!(parse("br, deflate", auto), ContentEncoding::Deflate);
        assert_eq!(parse("br", auto), ContentEncoding::Identity);
        assert_eq!(parse("gzip, br", ContentEncoding::Br), ContentEncoding::Br);
        assert_eq!(parse(" br ,  gzip ; q=1.0 ", auto), ContentEncoding::Gzip);
        assert_eq!(parse("", auto), ContentEncoding::Identity);
    }

    #[test]
    fn test_auto_honours_q() {
        let auto = ContentEncoding::Auto;
        assert_eq!(parse("gzip;q=0.5, zstd;q=0.1", auto), ContentEncoding::Gzip);
        assert_eq!(
            parse("deflate;q=0.9, gzip;q=0.8", auto),
            ContentEncoding::Deflate
        );
        assert_eq!(parse("zstd;q=0, gzip", auto), ContentEncoding::Gzip);
        assert_eq!(parse("gzip;q=0", auto), ContentEncoding::Identity);
        assert_eq!(parse("gzip;q=abc", auto), ContentEncoding::Identity);
        assert_eq!(parse("gzip;q=2, deflate", auto), ContentEncoding::Deflate);
        assert_eq!(parse("gzip, zstd;q=0", auto), ContentEncoding::Gzip);
        // the first entry for an encoding wins
        assert_eq!(parse("gzip;q=0, gzip", auto), ContentEncoding::Identity);
        // equal weights go to the server's preference
        assert_eq!(
            parse("deflate;q=0.5, gzip;q=0.5, zstd;q=0.5", auto),
            ContentEncoding::Zstd
        );
        assert_eq!(parse("deflate, gzip", auto), ContentEncoding::Gzip);
        assert_eq!(parse("x-gzip", auto), ContentEncoding::Gzip);
    }

    #[test]
    fn test_auto_wildcard() {
        let auto = ContentEncoding::Auto;
        assert_eq!(parse("*", auto), ContentEncoding::Zstd);
        assert_eq!(parse("*;q=0", auto), ContentEncoding::Identity);
        assert_eq!(parse("*;q=0.5, zstd;q=0", auto), ContentEncoding::Gzip);
        assert_eq!(
            parse("zstd;q=0, gzip;q=0, *", auto),
            ContentEncoding::Deflate
        );
        assert_eq!(parse("*;q=0.5, deflate", auto), ContentEncoding::Deflate);
        assert_eq!(parse("gzip;q=0.1, *;q=0.5", auto), ContentEncoding::Zstd);
        assert_eq!(parse("*;q=0, gzip;q=0.1", auto), ContentEncoding::Gzip);
        assert_eq!(parse("*;q=0.5, *", auto), ContentEncoding::Zstd);
        assert_eq!(parse("*;q=0, *, zstd;q=0", auto), ContentEncoding::Identity);
    }

    #[test]
    fn test_explicit_encoding() {
        let gzip = ContentEncoding::Gzip;
        assert_eq!(parse("gzip", gzip), gzip);
        assert_eq!(parse("zstd", gzip), ContentEncoding::Identity);
        assert_eq!(parse("gzip;q=0", gzip), ContentEncoding::Identity);
        assert_eq!(parse("gzip;q=0.001", gzip), gzip);
        assert_eq!(parse("*", gzip), gzip);
        assert_eq!(parse("*;q=0", gzip), ContentEncoding::Identity);
        assert_eq!(parse("*, gzip;q=0", gzip), ContentEncoding::Identity);
        assert_eq!(parse("*;q=0, gzip", gzip), gzip);
    }

    #[test]
    fn test_repeated_headers() {
        let values = ["zstd;q=0", "gzip;q=0.5, deflate;q=0.1"];
        assert_eq!(
            AcceptEncoding::parse(values.into_iter(), ContentEncoding::Auto),
            ContentEncoding::Gzip
        );
    }

    #[crate::rt_test]
    async fn test_compress_accept_encoding() {
        use crate::http::header::{CONTENT_ENCODING, HeaderValue};
        use crate::web::test::{TestRequest, call_service, init_service};
        use crate::web::{self, App, HttpResponse};

        let srv = init_service(App::new().middleware(Compress::default()).route(
            "/",
            web::get().to(async || HttpResponse::Ok().body("a".repeat(1024))),
        ))
        .await;

        let req = TestRequest::default()
            .header(ACCEPT_ENCODING, "gzip")
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.headers().get(CONTENT_ENCODING).unwrap(), "gzip");

        let req = TestRequest::default()
            .header(
                ACCEPT_ENCODING,
                HeaderValue::from_bytes(b"gzip\xff").unwrap(),
            )
            .to_request();
        let resp = call_service(&srv, req).await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());

        let req = TestRequest::default()
            .header(ACCEPT_ENCODING, "zstd;q=0")
            .header(ACCEPT_ENCODING, "gzip;q=0.5")
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.headers().get(CONTENT_ENCODING).unwrap(), "gzip");

        let req = TestRequest::default()
            .header(
                ACCEPT_ENCODING,
                HeaderValue::from_bytes(b"zstd\xff").unwrap(),
            )
            .header(ACCEPT_ENCODING, "deflate")
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.headers().get(CONTENT_ENCODING).unwrap(), "deflate");

        let req = TestRequest::default()
            .header(ACCEPT_ENCODING, "gzip;q=0")
            .to_request();
        let resp = call_service(&srv, req).await;
        assert!(resp.headers().get(CONTENT_ENCODING).is_none());
    }
}
