//! Test helpers for the ntex HTTP client.
#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use crate::http::error::HttpError;
use crate::http::header::{HeaderName, HeaderValue};
use crate::http::{Payload, ResponseHead, StatusCode, Version};
use crate::{Cfg, channel::bstream, util::Bytes};

use super::ClientResponse;

#[derive(Debug)]
/// Builder for creating a [`ClientResponse`] in tests.
pub struct TestResponse {
    head: ResponseHead,
    payload: Option<Payload>,
    #[cfg(feature = "cookie")]
    cookies: CookieJar,
}

impl TestResponse {
    /// Creates a test response builder.
    pub fn builder() -> TestResponse {
        TestResponse {
            head: ResponseHead::new(StatusCode::OK, Version::default()),
            payload: None,
            #[cfg(feature = "cookie")]
            cookies: CookieJar::new(),
        }
    }

    #[must_use]
    /// Creates a test response with one header.
    pub fn with_header<K, V>(key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
    {
        Self::builder().header(key, value)
    }

    #[must_use]
    /// Sets the response HTTP version.
    pub fn version(mut self, ver: Version) -> Self {
        self.head.version = ver;
        self
    }

    #[must_use]
    /// Appends a response header.
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
    {
        if let Ok(key) = HeaderName::try_from(key)
            && let Ok(value) = HeaderValue::try_from(value)
        {
            self.head.headers.append(key, value);
            return self;
        }
        panic!("Cannot create header");
    }

    #[must_use]
    #[cfg(feature = "cookie")]
    /// Adds a response cookie.
    pub fn cookie<C>(mut self, cookie: C) -> Self
    where
        C: Into<Cookie<'static>>,
    {
        self.cookies.add(cookie.into());
        self
    }

    #[must_use]
    /// Sets the response payload.
    pub fn set_payload<B: Into<Bytes>>(mut self, data: B) -> Self {
        self.payload = Some(bstream::empty(Some(data.into())).into());
        self
    }

    #[must_use]
    /// Builds the [`ClientResponse`].
    pub fn build(self) -> ClientResponse {
        #[allow(unused_mut)]
        let mut head = self.head;

        // one `Set-Cookie` header per cookie
        #[cfg(feature = "cookie")]
        for c in self.cookies.delta() {
            let value = HeaderValue::from_str(&c.encoded().to_string()).unwrap();
            head.headers.append(crate::http::header::SET_COOKIE, value);
        }

        if let Some(pl) = self.payload {
            ClientResponse::new(head, pl, Cfg::default())
        } else {
            ClientResponse::new(head, bstream::empty(None).into(), Cfg::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::header;

    #[crate::rt_test]
    async fn test_basics() {
        let res = {
            #[cfg(feature = "cookie")]
            {
                TestResponse::builder()
                    .version(Version::HTTP_2)
                    .header(header::DATE, "data")
                    .cookie(coo_kie::Cookie::build(("name", "value")))
                    .build()
            }
            #[cfg(not(feature = "cookie"))]
            {
                TestResponse::builder()
                    .version(Version::HTTP_2)
                    .header(header::DATE, "data")
                    .build()
            }
        };
        #[cfg(feature = "cookie")]
        assert!(res.headers().contains_key(header::SET_COOKIE));
        assert!(res.headers().contains_key(header::DATE));
        assert_eq!(res.version(), Version::HTTP_2);
    }

    #[cfg(feature = "cookie")]
    #[crate::rt_test]
    async fn test_cookies() {
        use crate::http::HttpMessage;
        use coo_kie::Cookie;

        let res = TestResponse::builder()
            .cookie(Cookie::build(("c1", "v 1")).path("/p"))
            .cookie(Cookie::build(("c2", "v2")))
            .build();
        assert_eq!(res.headers().get_all(header::SET_COOKIE).count(), 2);

        let mut cookies: Vec<_> = res
            .cookies()
            .unwrap()
            .iter()
            .map(|c| (c.name().to_string(), c.value().to_string()))
            .collect();
        cookies.sort_unstable();
        assert_eq!(
            cookies,
            [
                ("c1".to_string(), "v 1".to_string()),
                ("c2".to_string(), "v2".to_string())
            ]
        );
        assert_eq!(res.cookie("c1").unwrap().path(), Some("/p"));
    }
}
