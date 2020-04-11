use std::fmt;

use crate::http::body::{Body, MessageBody, ResponseBody};
use crate::http::{HeaderMap, Response, ResponseHead, StatusCode};

use super::error::{ErrorContainer, ErrorRenderer};
use super::httprequest::HttpRequest;

/// An service http response
pub struct WebResponse {
    request: HttpRequest,
    response: Response<Body>,
}

impl WebResponse {
    /// Create web response instance
    pub fn new(request: HttpRequest, response: Response<Body>) -> Self {
        WebResponse { request, response }
    }

    /// Create web response from the error
    pub fn from_err<Err: ErrorRenderer, E: Into<Err::Container>>(
        err: E,
        request: HttpRequest,
    ) -> Self {
        let err = err.into();
        let res: Response = err.error_response(&request);

        if res.head().status == StatusCode::INTERNAL_SERVER_ERROR {
            log::error!("Internal Server Error: {:?}", err);
        } else {
            log::debug!("Error in response: {:?}", err);
        }

        WebResponse {
            request,
            response: res.into_body(),
        }
    }

    /// Create web response for error
    #[inline]
    pub fn error_response<Err: ErrorRenderer, E: Into<Err::Container>>(
        self,
        err: E,
    ) -> Self {
        Self::from_err::<Err, E>(err, self.request)
    }

    /// Create web response
    #[inline]
    pub fn into_response(self, response: Response) -> WebResponse {
        WebResponse::new(self.request, response)
    }

    /// Get reference to original request
    #[inline]
    pub fn request(&self) -> &HttpRequest {
        &self.request
    }

    /// Get reference to response
    #[inline]
    pub fn response(&self) -> &Response<Body> {
        &self.response
    }

    /// Get mutable reference to response
    #[inline]
    pub fn response_mut(&mut self) -> &mut Response<Body> {
        &mut self.response
    }

    /// Get the response status code
    #[inline]
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }

    #[inline]
    /// Returns response's headers.
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    #[inline]
    /// Returns mutable response's headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        self.response.headers_mut()
    }

    /// Execute closure and in case of error convert it to response.
    pub fn checked_expr<F, E, Err>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut Self) -> Result<(), E>,
        E: Into<Err::Container>,
        Err: ErrorRenderer,
    {
        match f(&mut self) {
            Ok(_) => self,
            Err(err) => {
                let res: Response = err.into().into();
                WebResponse::new(self.request, res.into_body())
            }
        }
    }

    /// Extract response body
    pub fn take_body(&mut self) -> ResponseBody<Body> {
        self.response.take_body()
    }
}

impl WebResponse {
    /// Set a new body
    pub fn map_body<F>(self, f: F) -> WebResponse
    where
        F: FnOnce(&mut ResponseHead, ResponseBody<Body>) -> ResponseBody<Body>,
    {
        let response = self.response.map_body(f);

        WebResponse {
            response,
            request: self.request,
        }
    }
}

impl Into<Response<Body>> for WebResponse {
    fn into(self) -> Response<Body> {
        self.response
    }
}

impl fmt::Debug for WebResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = writeln!(
            f,
            "\nWebResponse {:?} {}{}",
            self.response.head().version,
            self.response.head().status,
            self.response.head().reason.unwrap_or(""),
        );
        let _ = writeln!(f, "  headers:");
        for (key, val) in self.response.head().headers.iter() {
            let _ = writeln!(f, "    {:?}: {:?}", key, val);
        }
        let _ = writeln!(f, "  body: {:?}", self.response.body().size());
        res
    }
}
