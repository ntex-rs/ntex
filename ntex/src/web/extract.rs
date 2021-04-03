//! Request extractors
use std::{future::Future, pin::Pin, task::Context, task::Poll};

use super::error::ErrorRenderer;
use super::httprequest::HttpRequest;
use crate::{http::Payload, util::Ready};

/// Trait implemented by types that can be extracted from request.
///
/// Types that implement this trait can be used with `Route` handlers.
pub trait FromRequest<Err>: Sized {
    /// The associated error which can be returned.
    type Error;

    /// Future that resolves to a Self
    type Future: Future<Output = Result<Self, Self::Error>>;

    /// Convert request to a Self
    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future;

    /// Convert request to a Self
    ///
    /// This method uses `Payload::None` as payload stream.
    fn extract(req: &HttpRequest) -> Self::Future {
        Self::from_request(req, &mut Payload::None)
    }
}

/// Optionally extract a field from the request
///
/// If the FromRequest for T fails, return None rather than returning an error response
///
/// ## Example
///
/// ```rust
/// use ntex::{http, util::Ready};
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
///     type Future = Ready<Self, Self::Error>;
///
///     fn from_request(req: &HttpRequest, payload: &mut http::Payload) -> Self::Future {
///         if rand::random() {
///             Ready::Ok(Thing { name: "thingy".into() })
///         } else {
///             Ready::Err(error::ErrorBadRequest("no luck").into())
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
    T: FromRequest<Err> + 'static,
    T::Future: 'static,
    Err: ErrorRenderer,
    <T as FromRequest<Err>>::Error: Into<Err::Container>,
{
    type Error = Err::Container;
    type Future = Pin<Box<dyn Future<Output = Result<Option<T>, Self::Error>>>>;

    #[inline]
    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let fut = T::from_request(req, payload);
        Box::pin(async move {
            match fut.await {
                Ok(v) => Ok(Some(v)),
                Err(e) => {
                    log::debug!("Error for Option<T> extractor: {}", e.into());
                    Ok(None)
                }
            }
        })
    }
}

/// Optionally extract a field from the request or extract the Error if unsuccessful
///
/// If the `FromRequest` for T fails, inject Err into handler rather than returning an error response
///
/// ## Example
///
/// ```rust
/// use ntex::{http, util::Ready};
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
///     type Future = Ready<Thing, Self::Error>;
///
///     fn from_request(req: &HttpRequest, payload: &mut http::Payload) -> Self::Future {
///         if rand::random() {
///             Ready::Ok(Thing { name: "thingy".into() })
///         } else {
///             Ready::Err(error::ErrorBadRequest("no luck").into())
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
    T: FromRequest<E> + 'static,
    T::Error: 'static,
    T::Future: 'static,
    E: ErrorRenderer,
{
    type Error = T::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Result<T, T::Error>, Self::Error>>>>;

    #[inline]
    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let fut = T::from_request(req, payload);
        Box::pin(async move {
            match fut.await {
                Ok(v) => Ok(Ok(v)),
                Err(e) => Ok(Err(e)),
            }
        })
    }
}

#[doc(hidden)]
impl<E: ErrorRenderer> FromRequest<E> for () {
    type Error = E::Container;
    type Future = Ready<(), E::Container>;

    fn from_request(_: &HttpRequest, _: &mut Payload) -> Self::Future {
        Ok(()).into()
    }
}

macro_rules! tuple_from_req ({$fut_type:ident, $(($n:tt, $T:ident)),+} => {

    /// FromRequest implementation for a tuple
    #[allow(unused_parens)]
    impl<Err: ErrorRenderer, $($T: FromRequest<Err> + 'static),+> FromRequest<Err> for ($($T,)+)
    where
        $(<$T as $crate::web::FromRequest<Err>>::Error: Into<Err::Container>),+
    {
        type Error = Err::Container;
        type Future = $fut_type<Err, $($T),+>;

        fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
            $fut_type {
                items: <($(Option<$T>,)+)>::default(),
                $($T: $T::from_request(req, payload),)+
            }
        }
    }

    pin_project_lite::pin_project! {
        #[doc(hidden)]
    pub struct $fut_type<Err: ErrorRenderer, $($T: FromRequest<Err>),+>
    {
        items: ($(Option<$T>,)+),
        $(#[pin] $T: $T::Future),+
    }
    }

    impl<Err: ErrorRenderer, $($T: FromRequest<Err>),+> Future for $fut_type<Err, $($T),+>
    where
        $(<$T as $crate::web::FromRequest<Err>>::Error: Into<Err::Container>),+
    {
        type Output = Result<($($T,)+), Err::Container>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();

            let mut ready = true;
            $(
                if this.items.$n.is_none() {
                    match this.$T.poll(cx) {
                        Poll::Ready(Ok(item)) => {
                            this.items.$n = Some(item);
                        }
                        Poll::Pending => ready = false,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                    }
                }
            )+

                if ready {
                    Poll::Ready(Ok(
                        ($(this.items.$n.take().unwrap(),)+)
                    ))
                } else {
                    Poll::Pending
                }
        }
    }
});

#[allow(non_snake_case)]
#[rustfmt::skip]
mod m {
    use super::*;

tuple_from_req!(TupleFromRequest1, (0, A));
tuple_from_req!(TupleFromRequest2, (0, A), (1, B));
tuple_from_req!(TupleFromRequest3, (0, A), (1, B), (2, C));
tuple_from_req!(TupleFromRequest4, (0, A), (1, B), (2, C), (3, D));
tuple_from_req!(TupleFromRequest5, (0, A), (1, B), (2, C), (3, D), (4, E));
tuple_from_req!(TupleFromRequest6, (0, A), (1, B), (2, C), (3, D), (4, E), (5, F));
tuple_from_req!(TupleFromRequest7, (0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G));
tuple_from_req!(TupleFromRequest8, (0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H));
tuple_from_req!(TupleFromRequest9, (0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H), (8, I));
tuple_from_req!(TupleFromRequest10, (0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H), (8, I), (9, J));
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use crate::http::header;
    use crate::web::error::UrlencodedError;
    use crate::web::test::{from_request, TestRequest};
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
        .data(FormConfig::default().limit(4096))
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
