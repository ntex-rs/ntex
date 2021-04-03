//! Test helpers for ntex http client to use during testing.
use std::convert::TryFrom;

use bytes::Bytes;

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use crate::http::error::HttpError;
use crate::http::header::{HeaderName, HeaderValue};
use crate::http::{h1, Payload, ResponseHead, StatusCode, Version};

use super::ClientResponse;

/// Test `ClientResponse` builder
pub struct TestResponse {
    head: ResponseHead,
    payload: Option<Payload>,
    #[cfg(feature = "cookie")]
    cookies: CookieJar,
}

impl Default for TestResponse {
    fn default() -> TestResponse {
        TestResponse {
            head: ResponseHead::new(StatusCode::OK),
            payload: None,
            #[cfg(feature = "cookie")]
            cookies: CookieJar::new(),
        }
    }
}

impl TestResponse {
    /// Create TestResponse and set header
    pub fn with_header<K, V>(key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
    {
        Self::default().header(key, value)
    }

    /// Set HTTP version of this response
    pub fn version(mut self, ver: Version) -> Self {
        self.head.version = ver;
        self
    }

    /// Append a header
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
    {
        if let Ok(key) = HeaderName::try_from(key) {
            if let Ok(value) = HeaderValue::try_from(value) {
                self.head.headers.append(key, value);
                return self;
            }
        }
        panic!("Cannot create header");
    }

    #[cfg(feature = "cookie")]
    /// Set cookie for this response
    pub fn cookie(mut self, cookie: Cookie<'_>) -> Self {
        self.cookies.add(cookie.into_owned());
        self
    }

    /// Set response's payload
    pub fn set_payload<B: Into<Bytes>>(mut self, data: B) -> Self {
        let mut payload = h1::Payload::empty();
        payload.unread_data(data.into());
        self.payload = Some(payload.into());
        self
    }

    /// Complete response creation and generate `ClientResponse` instance
    pub fn finish(self) -> ClientResponse {
        #[allow(unused_mut)]
        let mut head = self.head;

        #[cfg(feature = "cookie")]
        {
            use percent_encoding::percent_encode;
            use std::fmt::Write as FmtWrite;

            use crate::http::header;

            let mut cookie = String::new();
            for c in self.cookies.delta() {
                let name =
                    percent_encode(c.name().as_bytes(), crate::http::helpers::USERINFO);
                let value =
                    percent_encode(c.value().as_bytes(), crate::http::helpers::USERINFO);
                let _ = write!(&mut cookie, "; {}={}", name, value);
            }
            if !cookie.is_empty() {
                head.headers.insert(
                    header::SET_COOKIE,
                    HeaderValue::from_str(&cookie.as_str()[2..]).unwrap(),
                );
            }
        }

        if let Some(pl) = self.payload {
            ClientResponse::new(head, pl)
        } else {
            ClientResponse::new(head, h1::Payload::empty().into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::header;

    #[test]
    fn test_basics() {
        let res = {
            #[cfg(feature = "cookie")]
            {
                TestResponse::default()
                    .version(Version::HTTP_2)
                    .header(header::DATE, "data")
                    .cookie(coo_kie::Cookie::build("name", "value").finish())
                    .finish()
            }
            #[cfg(not(feature = "cookie"))]
            {
                TestResponse::default()
                    .version(Version::HTTP_2)
                    .header(header::DATE, "data")
                    .finish()
            }
        };
        #[cfg(feature = "cookie")]
        assert!(res.headers().contains_key(header::SET_COOKIE));
        assert!(res.headers().contains_key(header::DATE));
        assert_eq!(res.version(), Version::HTTP_2);
    }
}
