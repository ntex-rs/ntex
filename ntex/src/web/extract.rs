//! Request extractors
use std::convert::Infallible;

use super::{AppState, HttpRequest, HttpResponse, WebResponseError};
use crate::http::Payload;

#[allow(async_fn_in_trait)]
/// Trait implemented by types that can be extracted from request.
///
/// Types that implement this trait can be used with `Route` handlers.
pub trait FromRequest<St>: Sized {
    /// The associated error which can be returned.
    type Error;

    /// Convert request to a Self
    async fn from_request(
        st: &St,
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
/// use ntex::web::{self, error, App, HttpRequest, FromRequest, WebError};
/// use rand;
///
/// #[derive(Debug, serde::Deserialize)]
/// struct Thing {
///     name: String
/// }
///
/// impl<St> FromRequest<St> for Thing {
///     type Error = WebError;
///
///     async fn from_request(st: &St, req: &HttpRequest, payload: &mut http::Payload) -> Result<Self, Self::Error> {
///         if rand::random() {
///             Ok(Thing { name: "thingy".into() })
///         } else {
///             Err(WebError::new(error::ErrorBadRequest("no luck")))
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
impl<T, St> FromRequest<St> for Option<T>
where
    T: FromRequest<St>,
    St: AppState,
    <T as FromRequest<St>>::Error: WebResponseError<St::Error>,
{
    type Error = St::Error;

    #[inline]
    async fn from_request(
        st: &St,
        req: &HttpRequest,
        payload: &mut Payload,
    ) -> Result<Option<T>, Self::Error> {
        match T::from_request(st, req, payload).await {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                log::debug!("Error for Option<T> extractor: {e}");
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
/// use ntex::web::{self, error, App, AppState, HttpRequest, FromRequest, WebError};
/// use rand;
///
/// #[derive(Debug, serde::Deserialize)]
/// struct Thing {
///     name: String
/// }
///
/// impl<St: AppState> FromRequest<St> for Thing {
///     type Error = WebError;
///
///     async fn from_request(st: &St, req: &HttpRequest, payload: &mut http::Payload) -> Result<Thing, Self::Error> {
///         if rand::random() {
///             Ok(Thing { name: "thingy".into() })
///         } else {
///             Err(WebError::new(error::ErrorBadRequest("no luck")))
///         }
///     }
/// }
///
/// /// extract `Thing` from request
/// async fn index(supplied_thing: Result<Thing, error::WebError>) -> String {
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
impl<T, St> FromRequest<St> for Result<T, T::Error>
where
    T: FromRequest<St>,
    St: AppState,
{
    type Error = T::Error;

    #[inline]
    async fn from_request(
        st: &St,
        req: &HttpRequest,
        payload: &mut Payload,
    ) -> Result<Self, Self::Error> {
        match T::from_request(st, req, payload).await {
            Ok(v) => Ok(Ok(v)),
            Err(e) => Ok(Err(e)),
        }
    }
}

#[doc(hidden)]
impl<St: AppState> FromRequest<St> for () {
    type Error = Infallible;

    #[inline]
    async fn from_request(_: &St, _: &HttpRequest, _: &mut Payload) -> Result<(), Self::Error> {
        Ok(())
    }
}

macro_rules! tuple_from_req {
    ($(#[$meta:meta])* $(($T:ident, $t:ident)),*) => {
        $(#[$meta])*
        impl<St, $($T,)+> FromRequest<St> for ($($T,)+)
        where
            St: AppState,
            $($T: FromRequest<St> + 'static,)+
            $(<$T as $crate::web::FromRequest<St>>::Error: WebResponseError<St::Error>),+
        {
            type Error = HttpResponse;

            async fn from_request(st: &St, req: &HttpRequest, payload: &mut Payload) -> Result<($($T,)+), Self::Error> {
                Ok((
                    $($T::from_request(st, req, payload).await.map_err(|mut e| e.error_response(req))?,)+
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
        let (req, mut pl) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .state(FormConfig::default().limit(4096))
                .to_http_parts();

        let r = from_request::<_, Option<Form<Info>>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert_eq!(r, None);

        let (req, mut pl) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::CONTENT_LENGTH, "9")
                .set_payload(Bytes::from_static(b"hello=world"))
                .to_http_parts();

        let r = from_request::<_, Option<Form<Info>>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert_eq!(
            r,
            Some(Form(Info {
                hello: "world".into()
            }))
        );

        let (req, mut pl) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::CONTENT_LENGTH, "9")
                .set_payload(Bytes::from_static(b"bye=world"))
                .to_http_parts();

        let r = from_request::<_, Option<Form<Info>>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert_eq!(r, None);
    }

    #[crate::rt_test]
    async fn test_result() {
        let (req, mut pl) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::CONTENT_LENGTH, "11")
                .set_payload(Bytes::from_static(b"hello=world"))
                .to_http_parts();

        let r = from_request::<_, Result<Form<Info>, UrlencodedError>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert_eq!(
            r.unwrap(),
            Form(Info {
                hello: "world".into()
            })
        );

        let (req, mut pl) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::CONTENT_LENGTH, "9")
                .set_payload(Bytes::from_static(b"bye=world"))
                .to_http_parts();

        let r = from_request::<_, Result<Form<Info>, UrlencodedError>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert!(r.is_err());
    }
}
