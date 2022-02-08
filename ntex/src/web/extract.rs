//! Request extractors
use std::{convert::Infallible, fmt, future::Future, pin::Pin, task::Context, task::Poll};

use super::error::{Error, ErrorRenderer};
use super::httprequest::HttpRequest;
use crate::{http::Payload, util::Ready};

/// Trait implemented by types that can be extracted from request.
///
/// Types that implement this trait can be used with `Route` handlers.
pub trait FromRequest<'a, Err>: Sized {
    /// The associated error which can be returned.
    type Error;

    /// Future that resolves to a Self
    type Future: Future<Output = Result<Self, Self::Error>> + 'a;

    /// Convert request to a Self
    fn from_request(req: &'a HttpRequest, payload: &'a mut Payload) -> Self::Future;
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
impl<'a, T, Err> FromRequest<'a, Err> for Option<T>
where
    T: FromRequest<'a, Err> + 'static,
    T::Future: 'static,
    T::Error: fmt::Debug,
    Err: ErrorRenderer,
{
    type Error = T::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Option<T>, Self::Error>> + 'a>>;

    #[inline]
    fn from_request(req: &'a HttpRequest, payload: &'a mut Payload) -> Self::Future {
        let fut = T::from_request(req, payload);
        Box::pin(async move {
            match fut.await {
                Ok(v) => Ok(Some(v)),
                Err(e) => {
                    log::debug!("Error for Option<T> extractor: {:?}", e);
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
impl<'a, T, E> FromRequest<'a, E> for Result<T, T::Error>
where
    T: FromRequest<'a, E> + 'static,
    E: ErrorRenderer,
{
    type Error = T::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Result<T, T::Error>, Self::Error>> + 'a>>;

    #[inline]
    fn from_request(req: &'a HttpRequest, payload: &'a mut Payload) -> Self::Future {
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
impl<'a, E: ErrorRenderer> FromRequest<'a, E> for () {
    type Error = Infallible;
    type Future = Ready<(), Infallible>;

    fn from_request(_: &HttpRequest, _: &mut Payload) -> Self::Future {
        Ok(()).into()
    }
}

// /// FromRequest implementation for a tuple
// #[allow(unused_parens)]
// impl<'a, Err: ErrorRenderer, T> FromRequest<'a, Err> for (T,)
// where
//     T: FromRequest<'a, Err> + 'a,
//     T::Error: Into<Err::Container>,
// {
//     type Error = Err::Container;
//     type Future = TupleFromRequest1<'a, Err, T>;

//     fn from_request(req: &'a HttpRequest, payload: &'a mut Payload) -> Self::Future {
//         TupleFromRequest1 {
//             items: Default::default(),
//             fut1: Some(T::from_request(req, payload)),
//         }
//     }
// }

// pin_project_lite::pin_project! {
//     #[doc(hidden)]
//     pub struct TupleFromRequest1<'a, Err: ErrorRenderer, T: FromRequest<'a, Err>> {
//         items: (Option<T>,),
//         #[pin]
//         fut1: Option<T::Future>,
//     }
// }

// impl<'a, Err: ErrorRenderer, T: FromRequest<'a, Err>> Future for TupleFromRequest1<'a, Err, T>
// where
//     T: FromRequest<'a, Err> + 'a,
//     T::Error: Into<Err::Container>,
// {
//     type Output = Result<(T,), Err::Container>;

//     fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
//         let this = self.project();

//         let mut ready = true;
//         if this.items.0.is_none() {
//             match this.fut1.as_pin_mut().unwrap().poll(cx) {
//                 Poll::Ready(Ok(item)) => {
//                     this.items.0 = Some(item);
//                 }
//                 Poll::Pending => ready = false,
//                 Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
//             }
//         }

//         if ready {
//             Poll::Ready(Ok((this.items.0.take().unwrap(),)))
//         } else {
//             Poll::Pending
//         }
//     }
// }

macro_rules! tuple_from_req ({$fut_type:ident, $(($n:tt, $T:ident)),+} => {

    /// FromRequest implementation for a tuple
    #[allow(unused_parens)]
    impl<'a, Err: ErrorRenderer, $($T: FromRequest<'a, Err> + 'a),+> FromRequest<'a, Err> for ($($T,)+)
    where
        $(<$T as FromRequest<'a, Err>>::Error: Error<Err>),+
    {
        type Error = $crate::web::HttpResponse;
        type Future = $fut_type<'a, Err, $($T),+>;

        fn from_request(req: &'a HttpRequest, payload: &'a mut Payload) -> Self::Future {
            $fut_type {
                items: Default::default(),
                $($T: $T::from_request(req, unsafe { (payload as *mut Payload).as_mut().unwrap() }),)+
                    req, payload,
            }
        }
    }

    pin_project_lite::pin_project! {
        #[doc(hidden)]
        pub struct $fut_type<'a, Err: ErrorRenderer, $($T: FromRequest<'a, Err>),+>
        {
            req: &'a HttpRequest,
            payload: &'a mut Payload,
            items: ($(Option<$T>,)+),
            $(#[pin] $T: $T::Future),+
        }
    }

    impl<'a, Err: ErrorRenderer, $($T: FromRequest<'a, Err>),+> Future for $fut_type<'a, Err, $($T),+>
    where
        $(<$T as FromRequest<'a, Err>>::Error: Error<Err>),+
    {
        type Output = Result<($($T,)+), $crate::web::HttpResponse>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();

            $(
                if this.items.$n.is_none() {
                    match $crate::util::ready!(this.$T.poll(cx)) {
                        Ok(item) => {
                            this.items.$n = Some(item);
                        }
                        Err(e) => return Poll::Ready(Err(e.error_response(this.req))),
                    }
                }
            )+

                Poll::Ready(Ok(
                    ($(this.items.$n.take().unwrap(),)+)
                ))
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
    use crate::http::header;
    use crate::util::Bytes;
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
