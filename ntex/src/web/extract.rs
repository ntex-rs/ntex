//! Request extractors
use std::convert::Infallible;

use super::{HttpRequest, State, WebResponseError};
use crate::http::Payload;

/// Turns request data into a value that a handler can use.
///
/// Each argument accepted by a handler registered with
/// [`Route::to()`](super::Route::to) is an extractor. Before calling the
/// handler, ntex asks each argument type to create its value from the incoming
/// request.
///
/// An extractor can look at the application state, request headers, path, and
/// other request information. It can also read the request body through the
/// payload. Extractors run in the same order as the handler arguments and
/// share that payload, so an extractor that reads the body may leave nothing
/// for the next one. For that reason, a handler should normally have only one
/// body-reading extractor.
///
/// When an extractor returns an error, ntex turns it into an HTTP response and
/// skips the handler. The error must implement [`WebResponseError`] for the
/// application. If the handler should still run, use `Option<T>` to receive
/// `None`, or `Result<T, T::Error>` to receive the original error.
///
/// # Example
///
/// A custom extractor can turn a request header into a handler argument:
///
/// ```rust
/// use ntex::http::Payload;
/// use ntex::web::{self, FromRequest, HttpRequest, InternalError};
///
/// struct ClientName(String);
///
/// impl<St: web::State> FromRequest<St> for ClientName {
///     type Error = InternalError<&'static str>;
///
///     async fn from_request(_: &St, req: &HttpRequest, _: &mut Payload) -> Result<Self, Self::Error> {
///         req.headers()
///             .get("x-client-name")
///             .and_then(|value| value.to_str().ok())
///             .map(|value| ClientName(value.to_owned()))
///             .ok_or_else(|| web::error::ErrorBadRequest("Missing client name"))
///     }
/// }
///
/// async fn hello(client: ClientName) -> String {
///     format!("Hello, {}!", client.0)
/// }
///
/// let app = web::App::default().route("/", web::get().to(hello));
/// ```
pub trait FromRequest<St>: Sized {
    /// The error returned when extraction fails.
    ///
    /// For a route handler, ntex must be able to turn this error into an HTTP
    /// response through [`WebResponseError`].
    type Error;

    /// Creates the extractor value for this request.
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
/// use ntex::web::{self, error, App, HttpRequest, FromRequest, InternalError};
/// use rand;
///
/// #[derive(Debug, serde::Deserialize)]
/// struct Thing {
///     name: String
/// }
///
/// impl<St> FromRequest<St> for Thing {
///     type Error = InternalError<&'static str>;
///
///     async fn from_request(st: &St, req: &HttpRequest, payload: &mut http::Payload) -> Result<Self, Self::Error> {
///         if rand::random() {
///             Ok(Thing { name: "thingy".into() })
///         } else {
///             Err(error::ErrorBadRequest("no luck"))
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
///     let app = App::default().service(
///         web::resource("/users/:first").route(
///             web::post().to(index))
///     );
/// }
/// ```
impl<St, T> FromRequest<St> for Option<T>
where
    St: State,
    T: FromRequest<St>,
    <T as FromRequest<St>>::Error: WebResponseError<St, St::Error>,
{
    type Error = Infallible;

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
/// use ntex::web::{self, error, App, State, HttpRequest, FromRequest, InternalError};
/// use rand;
///
/// #[derive(Debug, serde::Deserialize)]
/// struct Thing {
///     name: String
/// }
///
/// impl<St: State> FromRequest<St> for Thing {
///     type Error = InternalError<&'static str>;
///
///     async fn from_request(st: &St, req: &HttpRequest, payload: &mut http::Payload) -> Result<Thing, Self::Error> {
///         if rand::random() {
///             Ok(Thing { name: "thingy".into() })
///         } else {
///             Err(error::ErrorBadRequest("no luck"))
///         }
///     }
/// }
///
/// /// extract `Thing` from request
/// async fn index(supplied_thing: Result<Thing, InternalError<&'static str>>) -> String {
///     match supplied_thing {
///         Ok(thing) => format!("Got thing: {:?}", thing),
///         Err(e) => format!("Error extracting thing: {}", e)
///     }
/// }
///
/// fn main() {
///     let app = App::default().service(
///         web::resource("/users/:first").route(web::post().to(index))
///     );
/// }
/// ```
impl<St, T> FromRequest<St> for Result<T, T::Error>
where
    St: State,
    T: FromRequest<St>,
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
impl<St: State> FromRequest<St> for () {
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
            St: State,
            $($T: FromRequest<St> + 'static,)+
            $(<$T as $crate::web::FromRequest<St>>::Error: WebResponseError<St, St::Error>),+
        {
            type Error = $crate::web::InternalError<&'static str>;

            async fn from_request(st: &St, req: &HttpRequest, payload: &mut Payload) -> Result<($($T,)+), Self::Error> {
                Ok((
                    $($T::from_request(st, req, payload).await.map_err(
                        |e|
                        $crate::web::InternalError::from_response("Error", e.error_response(st)))?,)+
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
        let (req, mut pl, ()) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .app_state(FormConfig::default().limit(4096))
                .to_http_parts();

        let r = from_request::<_, Option<Form<Info>>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert_eq!(r, None);

        let (req, mut pl, ()) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::CONTENT_LENGTH, "9")
                .payload(Bytes::from_static(b"hello=world"))
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

        let (req, mut pl, ()) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::CONTENT_LENGTH, "9")
                .payload(Bytes::from_static(b"bye=world"))
                .to_http_parts();

        let r = from_request::<_, Option<Form<Info>>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert_eq!(r, None);
    }

    #[crate::rt_test]
    async fn test_result() {
        let (req, mut pl, ()) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::CONTENT_LENGTH, "11")
                .payload(Bytes::from_static(b"hello=world"))
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

        let (req, mut pl, ()) =
            TestRequest::with_header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::CONTENT_LENGTH, "9")
                .payload(Bytes::from_static(b"bye=world"))
                .to_http_parts();

        let r = from_request::<_, Result<Form<Info>, UrlencodedError>>(&(), &req, &mut pl)
            .await
            .unwrap();
        assert!(r.is_err());
    }
}
