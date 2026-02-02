use std::{cell::Ref, cell::RefMut, fmt, mem, net};

use crate::http::header::{self, HeaderMap};
use crate::http::httpmessage::HttpMessage;
use crate::http::message::{Message, RequestHead};
use crate::http::{Method, Uri, Version, payload::Payload};
use crate::{io::IoRef, io::types, util::Extensions};

/// Request
pub struct Request {
    pub(crate) payload: Payload,
    pub(crate) head: Message<RequestHead>,
}

impl HttpMessage for Request {
    #[inline]
    fn message_headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    /// Request extensions
    #[inline]
    fn message_extensions(&self) -> Ref<'_, Extensions> {
        self.head.extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    fn message_extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.head.extensions_mut()
    }
}

impl From<Message<RequestHead>> for Request {
    fn from(head: Message<RequestHead>) -> Self {
        Request {
            head,
            payload: Payload::None,
        }
    }
}

impl Request {
    /// Create new Request instance
    pub fn new() -> Request {
        Request {
            head: Message::new(),
            payload: Payload::None,
        }
    }
}

impl Request {
    /// Create new Request instance
    pub fn with_payload(payload: Payload) -> Request {
        Request {
            payload,
            head: Message::new(),
        }
    }

    #[inline]
    /// Http message part of the request
    pub fn head(&self) -> &RequestHead {
        &self.head
    }

    #[inline]
    #[doc(hidden)]
    /// Mutable reference to a http message part of the request
    pub fn head_mut(&mut self) -> &mut RequestHead {
        &mut self.head
    }

    /// Request's uri.
    #[inline]
    pub fn uri(&self) -> &Uri {
        &self.head().uri
    }

    /// Mutable reference to the request's uri.
    #[inline]
    pub fn uri_mut(&mut self) -> &mut Uri {
        &mut self.head_mut().uri
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

    /// The target path of this Request.
    #[inline]
    pub fn path(&self) -> &str {
        self.head().uri.path()
    }

    #[inline]
    /// Request's headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    /// Mutable reference to the message's headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head_mut().headers
    }

    /// Check if request requires connection upgrade
    #[inline]
    pub fn upgrade(&self) -> bool {
        self.head().upgrade() || self.head().method == Method::CONNECT
    }

    /// Io reference for current connection
    #[inline]
    pub fn io(&self) -> Option<&IoRef> {
        self.head().io.as_ref()
    }

    /// Peer socket address
    ///
    /// Peer address is actual socket address, if proxy is used in front of
    /// ntex http server, then peer address would be address of this proxy.
    #[inline]
    pub fn peer_addr(&self) -> Option<net::SocketAddr> {
        self.head().io.as_ref().and_then(|io| {
            io.query::<types::PeerAddr>()
                .get()
                .map(types::PeerAddr::into_inner)
        })
    }

    /// Get request's payload
    pub fn payload(&mut self) -> &mut Payload {
        &mut self.payload
    }

    /// Get request's payload
    pub fn take_payload(&mut self) -> Payload {
        mem::take(&mut self.payload)
    }

    /// Replace request's payload, returns old one
    pub fn replace_payload(&mut self, payload: Payload) -> Payload {
        mem::replace(&mut self.payload, payload)
    }

    /// Request extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.head.extensions()
    }

    /// Mutable reference to a the request's extensions
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.head.extensions_mut()
    }

    /// Split request into request head and payload
    pub(crate) fn into_parts(self) -> (Message<RequestHead>, Payload) {
        (self.head, self.payload)
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "\nRequest {:?} {}:{}",
            self.version(),
            self.method(),
            self.path()
        )?;
        if let Some(q) = self.uri().query().as_ref() {
            writeln!(f, "  query: ?{q:?}")?;
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
    use super::*;

    #[test]
    fn test_basics() {
        let msg = Message::new();
        let mut req = Request::from(msg);
        assert!(req.io().is_none());

        req.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain"),
        );
        assert!(req.headers().contains_key(header::CONTENT_TYPE));

        req.extensions_mut()
            .insert(header::HeaderValue::from_static("text/plain"));
        assert_eq!(
            req.extensions().get::<header::HeaderValue>().unwrap(),
            header::HeaderValue::from_static("text/plain")
        );

        req.head_mut().headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("100"),
        );
        assert!(req.headers().contains_key(header::CONTENT_LENGTH));

        req.head_mut().no_chunking(true);
        assert!(!req.head().chunked());
        req.head_mut().no_chunking(false);
        assert!(req.head().chunked());

        *req.uri_mut() = Uri::try_from("/index.html?q=1").unwrap();
        assert_eq!(req.uri().path(), "/index.html");
        assert_eq!(req.uri().query(), Some("q=1"));

        let s = format!("{req:?}");
        assert!(s.contains("Request HTTP/1.1 GET:/index.html"));

        let s = format!("{:?}", req.head());
        assert!(s.contains("RequestHead { uri:"));
    }
}
