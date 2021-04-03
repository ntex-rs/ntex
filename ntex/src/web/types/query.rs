//! Query extractor

use std::{fmt, ops};

use serde::de;

use crate::web::error::{ErrorRenderer, QueryPayloadError};
use crate::web::{FromRequest, HttpRequest};
use crate::{http::Payload, util::Ready};

/// Extract typed information from the request's query.
///
/// **Note**: A query string consists of unordered `key=value` pairs, therefore it cannot
/// be decoded into any type which depends upon data ordering e.g. tuples or tuple-structs.
/// Attempts to do so will *fail at runtime*.
///
/// [**QueryConfig**](struct.QueryConfig.html) allows to configure extraction process.
///
/// ## Example
///
/// ```rust
/// use ntex::web;
///
/// #[derive(Debug, serde::Deserialize)]
/// pub enum ResponseType {
///    Token,
///    Code
/// }
///
/// #[derive(serde::Deserialize)]
/// pub struct AuthRequest {
///    id: u64,
///    response_type: ResponseType,
/// }
///
/// // Use `Query` extractor for query information (and destructure it within the signature).
/// // This handler gets called only if the request's query string contains a `username` field.
/// // The correct request for this handler would be `/index.html?id=64&response_type=Code"`.
/// async fn index(web::types::Query(info): web::types::Query<AuthRequest>) -> String {
///     format!("Authorization request for client with id={} and type={:?}!", info.id, info.response_type)
/// }
///
/// fn main() {
///     let app = web::App::new().service(
///        web::resource("/index.html").route(web::get().to(index))); // <- use `Query` extractor
/// }
/// ```
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub struct Query<T>(pub T);

impl<T> Query<T> {
    /// Deconstruct to a inner value
    pub fn into_inner(self) -> T {
        self.0
    }

    /// Get query parameters from the path
    pub fn from_query(query_str: &str) -> Result<Self, QueryPayloadError>
    where
        T: de::DeserializeOwned,
    {
        serde_urlencoded::from_str::<T>(query_str)
            .map(|val| Ok(Query(val)))
            .unwrap_or_else(move |e| Err(QueryPayloadError::Deserialize(e)))
    }
}

impl<T> ops::Deref for Query<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> ops::DerefMut for Query<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: fmt::Debug> fmt::Debug for Query<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: fmt::Display> fmt::Display for Query<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Extract typed information from the request's query.
///
/// ## Example
///
/// ```rust
/// use ntex::web;
///
/// #[derive(Debug, serde::Deserialize)]
/// pub enum ResponseType {
///    Token,
///    Code
/// }
///
/// #[derive(serde::Deserialize)]
/// pub struct AuthRequest {
///    id: u64,
///    response_type: ResponseType,
/// }
///
/// // Use `Query` extractor for query information.
/// // This handler get called only if request's query contains `username` field
/// // The correct request for this handler would be `/index.html?id=64&response_type=Code"`
/// async fn index(info: web::types::Query<AuthRequest>) -> String {
///     format!("Authorization request for client with id={} and type={:?}!", info.id, info.response_type)
/// }
///
/// fn main() {
///     let app = web::App::new().service(
///        web::resource("/index.html")
///            .route(web::get().to(index))); // <- use `Query` extractor
/// }
/// ```
impl<T, Err> FromRequest<Err> for Query<T>
where
    T: de::DeserializeOwned,
    Err: ErrorRenderer,
{
    type Error = QueryPayloadError;
    type Future = Ready<Self, Self::Error>;

    #[inline]
    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        serde_urlencoded::from_str::<T>(req.query_string())
            .map(|val| Ready::Ok(Query(val)))
            .unwrap_or_else(move |e| {
                let e = QueryPayloadError::Deserialize(e);

                log::debug!(
                    "Failed during Query extractor deserialization. \
                     Request path: {:?}",
                    req.path()
                );
                Ready::Err(e)
            })
    }
}

#[cfg(test)]
mod tests {
    use derive_more::Display;

    use super::*;
    use crate::web::test::{from_request, TestRequest};

    #[derive(serde::Deserialize, Debug, Display)]
    struct Id {
        id: String,
    }

    #[crate::rt_test]
    async fn test_service_request_extract() {
        let req = TestRequest::with_uri("/name/user1/").to_srv_request();
        assert!(Query::<Id>::from_query(&req.query_string()).is_err());

        let req = TestRequest::with_uri("/name/user1/?id=test").to_srv_request();
        let mut s = Query::<Id>::from_query(&req.query_string()).unwrap();

        assert_eq!(s.id, "test");
        assert_eq!(format!("{}, {:?}", s, s), "test, Id { id: \"test\" }");

        s.id = "test1".to_string();
        let s = s.into_inner();
        assert_eq!(s.id, "test1");
    }

    #[crate::rt_test]
    async fn test_request_extract() {
        let req = TestRequest::with_uri("/name/user1/").to_srv_request();
        let (req, mut pl) = req.into_parts();
        let res = from_request::<Query<Id>>(&req, &mut pl).await;
        assert!(res.is_err());

        let req = TestRequest::with_uri("/name/user1/?id=test").to_srv_request();
        let (req, mut pl) = req.into_parts();

        let mut s = from_request::<Query<Id>>(&req, &mut pl).await.unwrap();
        assert_eq!(s.id, "test");
        assert_eq!(format!("{}, {:?}", s, s), "test, Id { id: \"test\" }");

        s.id = "test1".to_string();
        let s = s.into_inner();
        assert_eq!(s.id, "test1");
    }
}
