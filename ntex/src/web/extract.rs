//! Request extractors
use super::error::ErrorRenderer;
use super::httprequest::HttpRequest;
use crate::http::Payload;

#[allow(async_fn_in_trait)]
/// Trait implemented by types that can be extracted from request.
///
/// Types that implement this trait can be used with `Route` handlers.
pub trait FromRequest<Err>: Sized {
    /// The associated error which can be returned.
    type Error;

    /// Convert request to a Self
    async fn from_request(
        req: &HttpRequest,
        payload: &mut Payload,
    ) -> Result<Self, Self::Error>;
}

/// Optionally extract a field from the request
///
/// If the `FromRequest` for T fails, return None rather than returning an error response
///
/// ## Example
///
/// ```rust
/// use ntex::http;
/// use ntex::web::{self, error, App, HttpRequest, FromRequest, DefaultError};
/// use rand;
///
/// #[derive(Debug, serde::Deserialize)]
/// struct Thing {
///     name: String
/// }
///
/// impl<Err> FromRequest<Err> for Thing {
///     type Error = error::Error;
///
///     async fn from_request(req: &HttpRequest, payload: &mut http::Payload) -> Result<Self, Self::Error> {
///         if rand::random() {
///             Ok(Thing { name: "thingy".into() })
///         } else {
///             Err(error::ErrorBadRequest("no luck").into())
///         }
///     }
/// }
///
/// /// extract `Thing` from request
/// async fn index(supplied_thing: Option<Thing>) -> String {
///     match supplied_thing {
///         // Puns not intended
///         Some(thing) => format!("Got something: {:?}", thing),
///         None => format!("No thing!")
///     }
/// }
///
/// fn main() {
///     let app = App::new().service(
///         web::resource("/users/:first").route(
///             web::post().to(index))
///     );
/// }
/// ```
impl<T, Err> FromRequest<Err> for Option<T>
where
    T: FromRequest<Err>,
    Err: ErrorRenderer,
    <T as FromRequest<Err>>::Error: Into<Err::Container>,
{
    type Error = Err::Container;

    #[inline]
    async fn from_request(
        req: &HttpRequest,
        payload: &mut Payload,
    ) -> Result<Option<T>, Self::Error> {
        match T::from_request(req, payload).await {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                log::debug!("Error for Option<T> extractor: {}", e.into());
                Ok(None)
            }
        }
    }
}

/// Optionally extract a field from the request or extract the Error if unsuccessful
///
/// If the `FromRequest` for T fails, inject Err into handler rather than returning an error response
///
/// ## Example
///
/// ```rust
/// use ntex::http;
/// use ntex::web::{self, error, App, HttpRequest, FromRequest};
/// use rand;
///
/// #[derive(Debug, serde::Deserialize)]
/// struct Thing {
///     name: String
/// }
///
/// impl<Err> FromRequest<Err> for Thing {
///     type Error = error::Error;
///
///     async fn from_request(req: &HttpRequest, payload: &mut http::Payload) -> Result<Thing, Self::Error> {
///         if rand::random() {
///             Ok(Thing { name: "thingy".into() })
///         } else {
///             Err(error::ErrorBadRequest("no luck").into())
///         }
///     }
/// }
///
/// /// extract `Thing` from request
/// async fn index(supplied_thing: Result<Thing, error::Error>) -> String {
///     match supplied_thing {
///         Ok(thing) => format!("Got thing: {:?}", thing),
///         Err(e) => format!("Error extracting thing: {}", e)
///     }
/// }
///
/// fn main() {
///     let app = App::new().service(
///         web::resource("/users/:first").route(web::post().to(index))
///     );
/// }
/// ```
impl<T, E> FromRequest<E> for Result<T, T::Error>
where
    T: FromRequest<E>,
    E: ErrorRenderer,
{
    type Error = T::Error;

    #[inline]
    async fn from_request(
        req: &HttpRequest,
        payload: &mut Payload,
    ) -> Result<Self, Self::Error> {
        match T::from_request(req, payload).await {
            Ok(v) => Ok(Ok(v)),
            Err(e) => Ok(Err(e)),
        }
    }
}

#[doc(hidden)]
impl<E: ErrorRenderer> FromRequest<E> for () {
    type Error = E::Container;

    #[inline]
    async fn from_request(_: &HttpRequest, _: &mut Payload) -> Result<(), E::Container> {
        Ok(())
    }
}

macro_rules! tuple_from_req {
    ($(#[$meta:meta])* $(($T:ident, $t:ident)),*) => {
        $(#[$meta])*
        impl<$($T,)+ Err: ErrorRenderer> FromRequest<Err> for ($($T,)+)
        where
            $($T: FromRequest<Err> + 'static,)+
            $(<$T as $crate::web::FromRequest<Err>>::Error: Into<Err::Container>),+
        {
            type Error = Err::Container;

            async fn from_request(req: &HttpRequest, payload: &mut Payload) -> Result<($($T,)+), Err::Container> {
                Ok((
                    $($T::from_request(req, payload).await.map_err(|e| e.into())?,)+
                ))
            }
        }
    }
}

#[allow(non_snake_case, clippy::wildcard_imports)]
#[rustfmt::skip]
mod m {
    use super::*;
    use variadics_please::all_tuples;

    all_tuples!(#[doc(fake_variadic)] tuple_from_req, 1, 12, T, t);
}

#[cfg(test)]
mod tests {
    use crate::http::header;
    use crate::util::Bytes;
    use crate::web::error::UrlencodedError;
    use crate::web::test::{TestRequest, from_request};
    use crate::web::types::{Form, FormConfig};

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Info {
        hello: String,
    }

    #[crate::rt_test]
    async fn test_option() {
        let (req, mut pl) = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .state(FormConfig::default().limit(4096))
        .to_http_parts();

        let r = from_request::<Option<Form<Info>>>(&req, &mut pl)
            .await
            .unwrap();
        assert_eq!(r, None);

        let (req, mut pl) = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(header::CONTENT_LENGTH, "9")
        .set_payload(Bytes::from_static(b"hello=world"))
        .to_http_parts();

        let r = from_request::<Option<Form<Info>>>(&req, &mut pl)
            .await
            .unwrap();
        assert_eq!(
            r,
            Some(Form(Info {
                hello: "world".into()
            }))
        );

        let (req, mut pl) = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(header::CONTENT_LENGTH, "9")
        .set_payload(Bytes::from_static(b"bye=world"))
        .to_http_parts();

        let r = from_request::<Option<Form<Info>>>(&req, &mut pl)
            .await
            .unwrap();
        assert_eq!(r, None);
    }

    #[crate::rt_test]
    async fn test_result() {
        let (req, mut pl) = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(header::CONTENT_LENGTH, "11")
        .set_payload(Bytes::from_static(b"hello=world"))
        .to_http_parts();

        let r = from_request::<Result<Form<Info>, UrlencodedError>>(&req, &mut pl)
            .await
            .unwrap();
        assert_eq!(
            r.unwrap(),
            Form(Info {
                hello: "world".into()
            })
        );

        let (req, mut pl) = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(header::CONTENT_LENGTH, "9")
        .set_payload(Bytes::from_static(b"bye=world"))
        .to_http_parts();

        let r = from_request::<Result<Form<Info>, UrlencodedError>>(&req, &mut pl)
            .await
            .unwrap();
        assert!(r.is_err());
    }
}
