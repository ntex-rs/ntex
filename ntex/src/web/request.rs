use std::{cell::Ref, cell::RefMut, fmt, marker::PhantomData, net, rc::Rc};

use crate::http::{
    HeaderMap, HttpMessage, Method, Payload, RequestHead, Response, Uri, Version, header,
};
use crate::io::{IoRef, types};
use crate::router::{Path, Resource};
use crate::{Cfg, util::Extensions};

use super::config::WebAppConfig;
use super::error::{ErrorRenderer, WebResponseError};
use super::httprequest::HttpRequest;
use super::info::ConnectionInfo;
use super::response::WebResponse;
use super::rmap::ResourceMap;
use super::service::AppState;

/// An service http request
///
/// `WebRequest` allows mutable access to request's internal structures
pub struct WebRequest<Err> {
    req: HttpRequest,
    _t: PhantomData<Err>,
}

impl<Err: ErrorRenderer> WebRequest<Err> {
    /// Create web response for error
    #[inline]
    pub fn render_error<E: WebResponseError<Err>>(self, err: E) -> WebResponse {
        WebResponse::new(err.error_response(&self.req), self.req)
    }

    /// Create web response for error
    #[inline]
    pub fn error_response<E: Into<Err::Container>>(self, err: E) -> WebResponse {
        WebResponse::from_err::<Err, E>(err, self.req)
    }
}

impl<Err> WebRequest<Err> {
    /// Construct web request
    pub(crate) fn new(req: HttpRequest) -> Self {
        WebRequest {
            req,
            _t: PhantomData,
        }
    }

    /// Deconstruct request into parts
    pub fn into_parts(mut self) -> (HttpRequest, Payload) {
        let pl = Rc::get_mut(&mut (self.req).0).unwrap().payload.take();
        (self.req, pl)
    }

    /// Construct request from parts.
    ///
    /// `WebRequest` can be re-constructed only if `req` hasnt been cloned.
    pub fn from_parts(
        mut req: HttpRequest,
        pl: Payload,
    ) -> Result<Self, (HttpRequest, Payload)> {
        if Rc::strong_count(&req.0) == 1 {
            Rc::get_mut(&mut req.0).unwrap().payload = pl;
            Ok(WebRequest::new(req))
        } else {
            Err((req, pl))
        }
    }

    /// Construct request from request.
    ///
    /// `HttpRequest` implements `Clone` trait via `Rc` type. `WebRequest`
    /// can be re-constructed only if rc's strong pointers count eq 1 and
    /// weak pointers count is 0.
    pub fn from_request(req: HttpRequest) -> Result<Self, HttpRequest> {
        if Rc::strong_count(&req.0) == 1 {
            Ok(WebRequest::new(req))
        } else {
            Err(req)
        }
    }

    /// Create web response
    #[inline]
    pub fn into_response<R: Into<Response>>(self, res: R) -> WebResponse {
        WebResponse::new(res.into(), self.req)
    }

    /// Io reference for current connection
    #[inline]
    pub fn io(&self) -> Option<&IoRef> {
        self.head().io.as_ref()
    }

    /// This method returns reference to the request head
    #[inline]
    pub fn head(&self) -> &RequestHead {
        self.req.head()
    }

    /// This method returns reference to the request head
    #[inline]
    pub fn head_mut(&mut self) -> &mut RequestHead {
        self.req.head_mut()
    }

    /// Request's uri.
    #[inline]
    pub fn uri(&self) -> &Uri {
        &self.head().uri
    }

    /// Read the Request method.
    #[inline]
    pub fn method(&self) -> &Method {
        &self.head().method
    }

    /// Read the Request Version.
    #[inline]
    pub fn version(&self) -> Version {
        self.head().version
    }

    #[inline]
    /// Returns request's headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    #[inline]
    /// Returns mutable request's headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head_mut().headers
    }

    /// The target path of this Request.
    #[inline]
    pub fn path(&self) -> &str {
        self.head().uri.path()
    }

    /// The query string in the URL.
    ///
    /// E.g., id=10
    #[inline]
    pub fn query_string(&self) -> &str {
        self.uri().query().unwrap_or_default()
    }

    /// Peer socket address
    ///
    /// Peer address is actual socket address, if proxy is used in front of
    /// ntex http server, then peer address would be address of this proxy.
    ///
    /// To get client connection information `ConnectionInfo` should be used.
    #[inline]
    pub fn peer_addr(&self) -> Option<net::SocketAddr> {
        self.head()
            .io
            .as_ref()
            .map(|io| io.query::<types::PeerAddr>().get().map(|addr| addr.0))
            .unwrap_or(None)
    }

    /// Get `ConnectionInfo` for the current request.
    #[inline]
    pub fn connection_info(&self) -> Ref<'_, ConnectionInfo> {
        ConnectionInfo::get(self.head(), self.app_config())
    }

    /// Get a reference to the Path parameters.
    ///
    /// Params is a container for url parameters.
    /// A variable segment is specified in the form `{identifier}`,
    /// where the identifier can be used later in a request handler to
    /// access the matched value for that segment.
    #[inline]
    pub fn match_info(&self) -> &Path<Uri> {
        self.req.match_info()
    }

    #[inline]
    /// Get a mutable reference to the Path parameters.
    pub fn match_info_mut(&mut self) -> &mut Path<Uri> {
        self.req.match_info_mut()
    }

    #[inline]
    /// Get a reference to a `ResourceMap` of current application.
    pub fn resource_map(&self) -> &ResourceMap {
        self.req.resource_map()
    }

    /// Service configuration
    #[inline]
    pub fn app_config(&self) -> &Cfg<WebAppConfig> {
        self.req.app_config()
    }

    #[inline]
    /// Get an application state stored with `App::app_state()` method during
    /// application configuration.
    ///
    /// To get state stored with `App::state()` use `web::types::State<T>` as type.
    pub fn app_state<T: 'static>(&self) -> Option<&T> {
        (self.req).0.app_state.get::<T>()
    }

    #[inline]
    /// Get request's payload
    pub fn take_payload(&mut self) -> Payload {
        Rc::get_mut(&mut (self.req).0).unwrap().payload.take()
    }

    #[inline]
    /// Set request payload.
    pub fn set_payload(&mut self, payload: Payload) {
        Rc::get_mut(&mut (self.req).0).unwrap().payload = payload;
    }

    /// Set new app state container
    pub(super) fn set_state_container(&mut self, state: AppState) {
        Rc::get_mut(&mut (self.req).0).unwrap().app_state = state;
    }

    /// Request extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.req.extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.req.extensions_mut()
    }
}

impl<Err> Resource<Uri> for WebRequest<Err> {
    fn path(&self) -> &str {
        self.match_info().path()
    }

    fn resource_path(&mut self) -> &mut Path<Uri> {
        self.match_info_mut()
    }
}

impl<Err> HttpMessage for WebRequest<Err> {
    #[inline]
    /// Returns Request's headers.
    fn message_headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    /// Request extensions
    #[inline]
    fn message_extensions(&self) -> Ref<'_, Extensions> {
        self.req.extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    fn message_extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.req.extensions_mut()
    }
}

impl<Err: ErrorRenderer> fmt::Debug for WebRequest<Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "\nWebRequest {:?} {}:{}",
            self.head().version,
            self.head().method,
            self.path()
        )?;
        if !self.query_string().is_empty() {
            writeln!(f, "  query: ?{:?}", self.query_string())?;
        }
        if !self.match_info().is_empty() {
            writeln!(f, "  params: {:?}", self.match_info())?;
        }
        writeln!(f, "  headers:")?;
        for (key, val) in self.headers() {
            if key == header::AUTHORIZATION {
                writeln!(f, "    {key:?}: <REDACTED>")?;
            } else {
                writeln!(f, "    {key:?}: {val:?}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::http::{self, HttpMessage, header};
    use crate::web::HttpResponse;
    use crate::web::test::TestRequest;

    #[test]
    fn test_request() {
        let mut req = TestRequest::default().to_srv_request();
        assert_eq!(req.head_mut().version, http::Version::HTTP_11);
        assert!(req.peer_addr().is_none());
        let err = http::error::PayloadError::Overflow;

        let res: HttpResponse = req.render_error(err).into();
        assert_eq!(res.status(), http::StatusCode::PAYLOAD_TOO_LARGE);

        let req = TestRequest::default().to_srv_request();
        let err = http::error::PayloadError::Overflow;

        let res: HttpResponse = req.error_response(err).into();
        assert_eq!(res.status(), http::StatusCode::PAYLOAD_TOO_LARGE);

        let mut req = TestRequest::default().to_srv_request();
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text"),
        );
        req.headers_mut().insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("text"),
        );
        req.headers_mut().remove(header::CONTENT_TYPE);
        assert!(!req.headers().contains_key(header::CONTENT_TYPE));
        assert!(!req.message_headers().contains_key(header::CONTENT_TYPE));

        let pl = req.take_payload();
        req.set_payload(pl);

        req.extensions_mut().insert("TEXT".to_string());
        assert_eq!(req.message_extensions().get::<String>().unwrap(), "TEXT");
        req.message_extensions_mut().remove::<String>();
        assert!(!req.extensions().contains::<String>());

        let t = format!("{req:?}");
        assert!(t.contains("\"authorization\": <REDACTED>"));
    }
}
