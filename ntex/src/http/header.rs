//! Various http headers

pub use ntex_http::header::{HeaderName, HeaderValue, InvalidHeaderValue};

pub use ntex_http::HeaderMap;
pub use ntex_http::header::*;
#[doc(hidden)]
pub use ntex_http::header::{AsName, GetAll, Value};

/// Represents supported types of content encodings
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ContentEncoding {
    /// Automatically select encoding based on encoding negotiation
    Auto,
    /// A format using the Brotli algorithm
    Br,
    /// A format using the zlib structure with deflate algorithm
    Deflate,
    /// Gzip algorithm
    Gzip,
    /// Indicates the identity function (i.e. no compression, nor modification)
    Identity,
}

impl ContentEncoding {
    #[inline]
    /// Is the content compressed?
    pub fn is_compressed(self) -> bool {
        !matches!(self, ContentEncoding::Identity | ContentEncoding::Auto)
    }

    #[inline]
    /// Convert content encoding to string
    pub fn as_str(self) -> &'static str {
        match self {
            ContentEncoding::Br => "br",
            ContentEncoding::Gzip => "gzip",
            ContentEncoding::Deflate => "deflate",
            ContentEncoding::Identity | ContentEncoding::Auto => "identity",
        }
    }

    #[inline]
    /// default quality value
    pub fn quality(self) -> f64 {
        match self {
            ContentEncoding::Br => 1.1,
            ContentEncoding::Gzip => 1.0,
            ContentEncoding::Deflate => 0.9,
            ContentEncoding::Identity | ContentEncoding::Auto => 0.1,
        }
    }
}

impl<'a> From<&'a str> for ContentEncoding {
    fn from(s: &'a str) -> ContentEncoding {
        let s = s.trim();

        if s.eq_ignore_ascii_case("br") {
            ContentEncoding::Br
        } else if s.eq_ignore_ascii_case("gzip") {
            ContentEncoding::Gzip
        } else if s.eq_ignore_ascii_case("deflate") {
            ContentEncoding::Deflate
        } else {
            ContentEncoding::Identity
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding() {
        assert!(ContentEncoding::Br.is_compressed());
        assert!(!ContentEncoding::Identity.is_compressed());
        assert!(!ContentEncoding::Auto.is_compressed());
        assert_eq!(format!("{:?}", ContentEncoding::Identity), "Identity");
    }
}
