use std::{cell::Ref, cell::RefMut, fmt, marker::PhantomData, net, rc::Rc};

use crate::http::{
    header, HeaderMap, HttpMessage, Method, Payload, RequestHead, Uri, Version,
};
use crate::io::{types, IoRef};
use crate::router::{Path, Resource};
use crate::util::Extensions;

use super::config::AppConfig;
use super::error::ErrorRenderer;
use super::httprequest::{HttpRequest, HttpRequestInner};
use super::info::ConnectionInfo;
use super::rmap::ResourceMap;

/// An service http request
///
/// WebRequest allows mutable access to request's internal structures
pub struct WebRequest<'a, Err> {
    pub(super) req: &'a mut HttpRequestInner,
    _marker: PhantomData<fn(&'a ()) -> &'a Err>,
}

impl<'a, Err> WebRequest<'a, Err> {
    /// Construct web request
    pub(super) fn new(req: &'a mut HttpRequestInner) -> Self {
        WebRequest {
            req,
            _marker: PhantomData,
        }
    }

    /// Http request
    pub fn http_request(&self) -> &HttpRequest {
        &self.req.request
    }

    /// Io reference for current connection
    #[inline]
    pub fn io(&self) -> Option<&IoRef> {
        self.head().io.as_ref()
    }

    /// This method returns reference to the request head
    #[inline]
    pub fn head(&self) -> &RequestHead {
        self.req.request.head()
    }

    /// This method returns reference to the request head
    #[inline]
    pub fn head_mut(&mut self) -> &mut RequestHead {
        self.req.request.head_mut()
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
        if let Some(query) = self.uri().query().as_ref() {
            query
        } else {
            ""
        }
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

    /// Get *ConnectionInfo* for the current request.
    #[inline]
    pub fn connection_info(&self) -> Ref<'_, ConnectionInfo> {
        ConnectionInfo::get(self.head(), &*self.app_config())
    }

    /// Get a reference to the Path parameters.
    ///
    /// Params is a container for url parameters.
    /// A variable segment is specified in the form `{identifier}`,
    /// where the identifier can be used later in a request handler to
    /// access the matched value for that segment.
    #[inline]
    pub fn match_info(&self) -> &Path<Uri> {
        self.req.request.match_info()
    }

    #[inline]
    /// Get a mutable reference to the Path parameters.
    pub fn match_info_mut(&mut self) -> &mut Path<Uri> {
        self.req.request.match_info_mut()
    }

    #[inline]
    /// Get a reference to a `ResourceMap` of current application.
    pub fn resource_map(&self) -> &ResourceMap {
        self.req.request.resource_map()
    }

    /// Service configuration
    #[inline]
    pub fn app_config(&self) -> &AppConfig {
        self.req.request.app_config()
    }

    #[inline]
    /// Get an application state stored with `App::app_state()` method during
    /// application configuration.
    ///
    /// To get state stored with `App::state()` use `web::types::State<T>` as type.
    pub fn app_state<T: 'static>(&self) -> Option<&T> {
        self.req.request.app_state.get::<T>()
    }

    #[inline]
    /// Get request's payload
    pub fn payload(&mut self) -> &Payload {
        &self.req.payload
    }

    #[inline]
    /// Get mutable request's payload
    pub fn payload_mut(&mut self) -> &mut Payload {
        &mut self.req.payload
    }

    #[inline]
    /// Get request's payload
    pub fn take_payload(&mut self) -> Payload {
        self.req.payload.take()
    }

    #[inline]
    /// Set request payload.
    pub fn set_payload(&mut self, payload: Payload) {
        self.req.payload = payload;
    }

    #[doc(hidden)]
    /// Set new app state container
    pub fn set_state_container(&mut self, extensions: Rc<Extensions>) {
        self.req.request.app_state = extensions;
    }

    /// Request extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.req.request.extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.req.request.extensions_mut()
    }

    #[inline]
    /// Get request's payload
    pub(super) fn with_params<F, R>(&'a mut self, f: F) -> R
    where
        F: FnOnce(&'a HttpRequest, &'a mut Payload) -> R,
        R: 'a,
    {
        f(&self.req.request, &mut self.req.payload)
    }
}

impl<'a, Err> Resource<Uri> for WebRequest<'a, Err> {
    fn path(&self) -> &str {
        self.match_info().path()
    }

    fn resource_path(&mut self) -> &mut Path<Uri> {
        self.match_info_mut()
    }
}

impl<'a, Err> HttpMessage for WebRequest<'a, Err> {
    #[inline]
    /// Returns Request's headers.
    fn message_headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    /// Request extensions
    #[inline]
    fn message_extensions(&self) -> Ref<'_, Extensions> {
        self.req.request.extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    fn message_extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.req.request.extensions_mut()
    }
}

impl<'a, Err: ErrorRenderer> fmt::Debug for WebRequest<'a, Err> {
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
        for (key, val) in self.headers().iter() {
            if key == header::AUTHORIZATION {
                writeln!(f, "    {:?}: <REDACTED>", key)?;
            } else {
                writeln!(f, "    {:?}: {:?}", key, val)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::http::{self, header, HttpMessage};
    use crate::web::test::TestRequest;
    use crate::web::HttpResponse;

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
        req.headers_mut().remove(header::CONTENT_TYPE);
        assert!(!req.headers().contains_key(header::CONTENT_TYPE));
        assert!(!req.message_headers().contains_key(header::CONTENT_TYPE));

        req.extensions_mut().insert("TEXT".to_string());
        assert_eq!(req.message_extensions().get::<String>().unwrap(), "TEXT");
        req.message_extensions_mut().remove::<String>();
        assert!(!req.extensions().contains::<String>());
    }
}
