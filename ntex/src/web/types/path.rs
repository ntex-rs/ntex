//! Path extractor
use std::{fmt, ops};

use serde::de;

use crate::web::error::{ErrorRenderer, PathError};
use crate::web::{FromRequest, HttpRequest};
use crate::{http::Payload, router::PathDeserializer, util::Ready};

#[derive(PartialEq, Eq, PartialOrd, Ord)]
/// Extract typed information from the request's path.
///
/// [**PathConfig**](struct.PathConfig.html) allows to configure extraction process.
///
/// ## Example
///
/// ```rust
/// use ntex::web;
///
/// /// extract path info from "/{username}/{count}/index.html" url
/// /// {username} - deserializes to a String
/// /// {count} -  - deserializes to a u32
/// async fn index(info: web::types::Path<(String, u32)>) -> String {
///     format!("Welcome {}! {}", info.0, info.1)
/// }
///
/// fn main() {
///     let app = web::App::new().service(
///         web::resource("/{username}/{count}/index.html") // <- define path parameters
///              .route(web::get().to(index))               // <- register handler with `Path` extractor
///     );
/// }
/// ```
///
/// It is possible to extract path information to a specific type that
/// implements `Deserialize` trait from *serde*.
///
/// ```rust
/// use ntex::web;
///
/// #[derive(serde::Deserialize)]
/// struct Info {
///     username: String,
/// }
///
/// /// extract `Info` from a path using serde
/// async fn index(info: web::types::Path<Info>) -> Result<String, web::Error> {
///     Ok(format!("Welcome {}!", info.username))
/// }
///
/// fn main() {
///     let app = web::App::new().service(
///         web::resource("/{username}/index.html") // <- define path parameters
///              .route(web::get().to(index)) // <- use handler with Path` extractor
///     );
/// }
/// ```
pub struct Path<T> {
    inner: T,
}

impl<T> Path<T> {
    /// Deconstruct to an inner value
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> AsRef<T> for Path<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

impl<T> ops::Deref for Path<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> ops::DerefMut for Path<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> From<T> for Path<T> {
    fn from(inner: T) -> Path<T> {
        Path { inner }
    }
}

impl<T: fmt::Debug> fmt::Debug for Path<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<T: fmt::Display> fmt::Display for Path<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// Extract typed information from the request's path.
///
/// ## Example
///
/// ```rust
/// use ntex::web;
///
/// /// extract path info from "/{username}/{count}/index.html" url
/// /// {username} - deserializes to a String
/// /// {count} -  - deserializes to a u32
/// async fn index(info: web::types::Path<(String, u32)>) -> String {
///     format!("Welcome {}! {}", info.0, info.1)
/// }
///
/// fn main() {
///     let app = web::App::new().service(
///         web::resource("/{username}/{count}/index.html") // <- define path parameters
///              .route(web::get().to(index)) // <- register handler with `Path` extractor
///     );
/// }
/// ```
///
/// It is possible to extract path information to a specific type that
/// implements `Deserialize` trait from *serde*.
///
/// ```rust
/// use ntex::web;
///
/// #[derive(serde::Deserialize)]
/// struct Info {
///     username: String,
/// }
///
/// /// extract `Info` from a path using serde
/// async fn index(info: web::types::Path<Info>) -> Result<String, web::Error> {
///     Ok(format!("Welcome {}!", info.username))
/// }
///
/// fn main() {
///     let app = web::App::new().service(
///         web::resource("/{username}/index.html") // <- define path parameters
///              .route(web::get().to(index)) // <- use handler with Path` extractor
///     );
/// }
/// ```
impl<T, Err: ErrorRenderer> FromRequest<Err> for Path<T>
where
    T: de::DeserializeOwned,
{
    type Error = PathError;
    type Future = Ready<Self, Self::Error>;

    #[inline]
    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        Ready::from(
            de::Deserialize::deserialize(PathDeserializer::new(req.match_info()))
                .map(|inner| Path { inner })
                .map_err(move |e| {
                    log::debug!(
                        "Failed during Path extractor deserialization. \
                         Request path: {:?}",
                        req.path()
                    );
                    PathError::from(e)
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use derive_more::Display;

    use super::*;
    use crate::router::Router;
    use crate::web::test::{from_request, TestRequest};

    #[derive(serde::Deserialize, Debug, Display)]
    #[display(fmt = "MyStruct({}, {})", key, value)]
    struct MyStruct {
        key: String,
        value: String,
    }

    #[derive(serde::Deserialize)]
    struct Test2 {
        key: String,
        value: u32,
    }

    #[crate::rt_test]
    async fn test_extract_path_single() {
        let mut router = Router::<usize>::build();
        router.path("/{value}/", 10).0.set_id(0);
        let router = router.finish();

        let mut req = TestRequest::with_uri("/32/").to_srv_request();
        router.recognize(req.match_info_mut());

        let (req, mut pl) = req.into_parts();
        assert_eq!(*from_request::<Path<i8>>(&req, &mut pl).await.unwrap(), 32);
        assert!(from_request::<Path<MyStruct>>(&req, &mut pl).await.is_err());
    }

    #[crate::rt_test]
    async fn test_tuple_extract() {
        let mut router = Router::<usize>::build();
        router.path("/{key}/{value}/", 10).0.set_id(0);
        let router = router.finish();

        let mut req = TestRequest::with_uri("/name/user1/?id=test").to_srv_request();
        router.recognize(req.match_info_mut());

        let (req, mut pl) = req.into_parts();
        let res = from_request::<(Path<(String, String)>,)>(&req, &mut pl)
            .await
            .unwrap();
        assert_eq!((res.0).0, "name");
        assert_eq!((res.0).1, "user1");

        let res = from_request::<(Path<(String, String)>, Path<(String, String)>)>(
            &req, &mut pl,
        )
        .await
        .unwrap();
        assert_eq!((res.0).0, "name");
        assert_eq!((res.0).1, "user1");
        assert_eq!((res.1).0, "name");
        assert_eq!((res.1).1, "user1");

        from_request::<()>(&req, &mut pl).await.unwrap();
    }

    #[crate::rt_test]
    async fn test_request_extract() {
        let mut router = Router::<usize>::build();
        router.path("/{key}/{value}/", 10).0.set_id(0);
        let router = router.finish();

        let mut req = TestRequest::with_uri("/name/user1/?id=test").to_srv_request();
        router.recognize(req.match_info_mut());

        let (req, mut pl) = req.into_parts();
        let mut s = from_request::<Path<MyStruct>>(&req, &mut pl).await.unwrap();
        assert_eq!(s.key, "name");
        assert_eq!(s.value, "user1");
        s.value = "user2".to_string();
        assert_eq!(s.value, "user2");
        assert_eq!(
            format!("{}, {:?}", s, s),
            "MyStruct(name, user2), MyStruct { key: \"name\", value: \"user2\" }"
        );
        let s = s.into_inner();
        assert_eq!(s.value, "user2");

        let s = from_request::<Path<(String, String)>>(&req, &mut pl)
            .await
            .unwrap();
        assert_eq!(s.0, "name");
        assert_eq!(s.1, "user1");

        let mut req = TestRequest::with_uri("/name/32/").to_srv_request();
        router.recognize(req.match_info_mut());

        let (req, mut pl) = req.into_parts();
        let s = from_request::<Path<Test2>>(&req, &mut pl).await.unwrap();
        assert_eq!(s.as_ref().key, "name");
        assert_eq!(s.value, 32);

        let s = from_request::<Path<(String, u8)>>(&req, &mut pl)
            .await
            .unwrap();
        assert_eq!(s.0, "name");
        assert_eq!(s.1, 32);

        let res = from_request::<Path<Vec<String>>>(&req, &mut pl)
            .await
            .unwrap();
        assert_eq!(res[0], "name".to_owned());
        assert_eq!(res[1], "32".to_owned());
    }
}
