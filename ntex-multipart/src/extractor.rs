//! Multipart payload support
use ntex::web::{WebError, FromRequest, HttpRequest};
use ntex::http::Payload;
use futures::future::{ok, Ready};

use crate::server::Multipart;

/// Get request's payload as multipart stream
///
/// Content-type: multipart/form-data;
///
impl<E: 'static> FromRequest<E> for Multipart {
    type Error = WebError<E>;
    type Future = Ready<Result<Self, Self::Error>>;
    type Config = ();

    #[inline]
    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        ok(Multipart::new(req.headers(), payload.take()))
    }
}

