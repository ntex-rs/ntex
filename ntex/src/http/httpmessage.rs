use std::cell::{Ref, RefMut};
use std::str;

use encoding_rs::{Encoding, UTF_8};
use mime::Mime;
use ntex_http::header;

#[cfg(feature = "cookie")]
use coo_kie::Cookie;

use super::error::{ContentTypeError, DecodeError};
use super::header::HeaderMap;
use crate::util::Extensions;

#[cfg(feature = "cookie")]
struct Cookies(Vec<Cookie<'static>>);

/// Trait that implements general purpose operations on http messages
pub trait HttpMessage: Sized {
    /// Read the message headers.
    fn message_headers(&self) -> &HeaderMap;

    /// Request's extensions container
    fn message_extensions(&self) -> Ref<'_, Extensions>;

    /// Mutable reference to a the request's extensions container
    fn message_extensions_mut(&self) -> RefMut<'_, Extensions>;

    /// Read the request content type. If request does not contain
    /// *Content-Type* header, empty str get returned.
    fn content_type(&self) -> &str {
        if let Some(content_type) = self.message_headers().get(header::CONTENT_TYPE)
            && let Ok(content_type) = content_type.to_str()
        {
            return content_type.split(';').next().unwrap().trim();
        }
        ""
    }

    /// Get content type encoding
    ///
    /// UTF-8 is used by default, If request charset is not set.
    fn encoding(&self) -> Result<&'static Encoding, ContentTypeError> {
        if let Some(mime_type) = self.mime_type()? {
            if let Some(charset) = mime_type.get_param("charset") {
                if let Some(enc) =
                    Encoding::for_label_no_replacement(charset.as_str().as_bytes())
                {
                    Ok(enc)
                } else {
                    Err(ContentTypeError::UnknownEncoding)
                }
            } else {
                Ok(UTF_8)
            }
        } else {
            Ok(UTF_8)
        }
    }

    /// Convert the request content type to a known mime type.
    fn mime_type(&self) -> Result<Option<Mime>, ContentTypeError> {
        if let Some(content_type) = self.message_headers().get(header::CONTENT_TYPE) {
            if let Ok(content_type) = content_type.to_str() {
                return match content_type.parse() {
                    Ok(mt) => Ok(Some(mt)),
                    Err(_) => Err(ContentTypeError::ParseError),
                };
            } else {
                return Err(ContentTypeError::ParseError);
            }
        }
        Ok(None)
    }

    /// Check if request has chunked transfer encoding
    fn chunked(&self) -> Result<bool, DecodeError> {
        if let Some(encodings) = self.message_headers().get(header::TRANSFER_ENCODING) {
            if let Ok(s) = encodings.to_str() {
                Ok(s.to_lowercase().contains("chunked"))
            } else {
                Err(DecodeError::Header)
            }
        } else {
            Ok(false)
        }
    }

    #[cfg(feature = "cookie")]
    /// Load request cookies.
    fn cookies(&self) -> Result<Ref<'_, Vec<Cookie<'static>>>, coo_kie::ParseError> {
        if self.message_extensions().get::<Cookies>().is_none() {
            let mut cookies = Vec::new();
            for hdr in self.message_headers().get_all(header::COOKIE) {
                let s =
                    str::from_utf8(hdr.as_bytes()).map_err(coo_kie::ParseError::from)?;
                for cookie_str in s.split(';').map(|s| s.trim()) {
                    if !cookie_str.is_empty() {
                        cookies.push(Cookie::parse_encoded(cookie_str)?.into_owned());
                    }
                }
            }
            self.message_extensions_mut().insert(Cookies(cookies));
        }
        Ok(Ref::map(self.message_extensions(), |ext| {
            &ext.get::<Cookies>().unwrap().0
        }))
    }

    #[cfg(feature = "cookie")]
    /// Return request cookie.
    fn cookie(&self, name: &str) -> Option<Cookie<'static>> {
        if let Ok(cookies) = self.cookies() {
            for cookie in cookies.iter() {
                if cookie.name() == name {
                    return Some(cookie.to_owned());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use encoding_rs::ISO_8859_2;

    use super::*;
    use crate::http::test::TestRequest;

    #[test]
    fn test_content_type() {
        let req = TestRequest::with_header("content-type", "text/plain").finish();
        assert_eq!(req.content_type(), "text/plain");
        let req =
            TestRequest::with_header("content-type", "application/json; charset=utf=8")
                .finish();
        assert_eq!(req.content_type(), "application/json");
        let req = TestRequest::default().finish();
        assert_eq!(req.content_type(), "");
    }

    #[test]
    fn test_mime_type() {
        let req = TestRequest::with_header("content-type", "application/json").finish();
        assert_eq!(req.mime_type().unwrap(), Some(mime::APPLICATION_JSON));
        let req = TestRequest::default().finish();
        assert_eq!(req.mime_type().unwrap(), None);
        let req =
            TestRequest::with_header("content-type", "application/json; charset=utf-8")
                .finish();
        let mt = req.mime_type().unwrap().unwrap();
        assert_eq!(mt.get_param(mime::CHARSET), Some(mime::UTF_8));
        assert_eq!(mt.type_(), mime::APPLICATION);
        assert_eq!(mt.subtype(), mime::JSON);
    }

    #[test]
    fn test_mime_type_error() {
        let req = TestRequest::with_header(
            "content-type",
            "applicationadfadsfasdflknadsfklnadsfjson",
        )
        .finish();
        assert_eq!(Err(ContentTypeError::ParseError), req.mime_type());
    }

    #[test]
    fn test_encoding() {
        let req = TestRequest::default().finish();
        assert_eq!(UTF_8.name(), req.encoding().unwrap().name());

        let req = TestRequest::with_header("content-type", "application/json").finish();
        assert_eq!(UTF_8.name(), req.encoding().unwrap().name());

        let req = TestRequest::with_header(
            "content-type",
            "application/json; charset=ISO-8859-2",
        )
        .finish();
        assert_eq!(ISO_8859_2, req.encoding().unwrap());
    }

    #[test]
    fn test_encoding_error() {
        let req = TestRequest::with_header("content-type", "applicatjson").finish();
        assert_eq!(Some(ContentTypeError::ParseError), req.encoding().err());

        let req =
            TestRequest::with_header("content-type", "application/json; charset=kkkttktk")
                .finish();
        assert_eq!(
            Some(ContentTypeError::UnknownEncoding),
            req.encoding().err()
        );
    }

    #[test]
    fn test_chunked() {
        let req = TestRequest::default().finish();
        assert!(!req.chunked().unwrap());

        let req = TestRequest::with_header(header::TRANSFER_ENCODING, "chunked").finish();
        assert!(req.chunked().unwrap());

        let req = TestRequest::default()
            .header(
                header::TRANSFER_ENCODING,
                b"some va\xadscc\xacas0xsdasdlue".as_ref(),
            )
            .finish();
        assert!(req.chunked().is_err());
    }
}
