use std::{cell::Ref, cell::RefCell, cell::RefMut, fmt, net, rc::Rc};

use bitflags::bitflags;

use crate::http::header::HeaderMap;
use crate::http::{HeaderItem, Method, StatusCode, Uri, Version, h1::Codec};
use crate::io::{IoBoxed, IoRef, types};
use crate::util::Extensions;

/// The connection behavior selected for an HTTP message.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ConnectionType {
    /// Close connection after response
    Close,
    /// Keep connection alive after response
    KeepAlive,
    /// Connection is upgraded to different type
    Upgrade,
}

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub(crate) struct Flags: u8 {
        const CLOSE       = 0b0000_0001;
        const KEEP_ALIVE  = 0b0000_0010;
        const UPGRADE     = 0b0000_0100;
        const EXPECT      = 0b0000_1000;
        const NO_CHUNKING = 0b0001_0000;
    }
}

pub(crate) trait Head: Default + 'static + fmt::Debug {
    fn clear(&mut self);

    /// Calls `f` with the thread's pool, returns `None` if the pool is already destroyed.
    fn with_pool<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&MessagePool<Self>) -> R;
}

#[derive(Clone, Debug)]
pub(crate) enum CurrentIo {
    Ref(IoRef),
    Io(Rc<dyn IoAccess>),
    None,
}

pub(crate) trait IoAccess: fmt::Debug {
    fn get(&self) -> Option<&IoRef>;

    fn take(&self) -> Option<(IoBoxed, Codec)>;
}

impl CurrentIo {
    pub(crate) fn new(io: Rc<dyn IoAccess>) -> Self {
        CurrentIo::Io(io)
    }

    pub(crate) fn as_ref(&self) -> Option<&IoRef> {
        match self {
            CurrentIo::Ref(io) => Some(io),
            CurrentIo::Io(io) => io.get(),
            CurrentIo::None => None,
        }
    }

    pub(crate) fn take(&self) -> Option<(IoBoxed, Codec)> {
        match self {
            CurrentIo::Io(io) => io.take(),
            _ => None,
        }
    }
}

/// The parsed metadata for an HTTP request.
#[derive(Debug)]
pub struct RequestHead {
    /// Identifier of the connection that received the request.
    pub id: usize,
    /// Request URI.
    pub uri: Uri,
    /// Request method.
    pub method: Method,
    /// HTTP protocol version.
    pub version: Version,
    /// Parsed request headers.
    pub headers: HeaderMap,
    /// Headers in their original order and with their original names.
    ///
    /// This collection is populated only when
    /// [`HttpServiceConfig::set_headers_vec`](crate::http::HttpServiceConfig::set_headers_vec)
    /// is enabled.
    pub headers_vec: Vec<HeaderItem>,
    /// Request-local type map.
    pub extensions: RefCell<Extensions>,
    pub(crate) io: CurrentIo,
    pub(crate) flags: Flags,
}

impl Default for RequestHead {
    fn default() -> RequestHead {
        RequestHead {
            id: 0,
            io: CurrentIo::None,
            uri: Uri::default(),
            method: Method::default(),
            version: Version::HTTP_11,
            headers: HeaderMap::with_capacity(16),
            headers_vec: Vec::default(),
            flags: Flags::empty(),
            extensions: RefCell::new(Extensions::new()),
        }
    }
}

impl Head for RequestHead {
    fn clear(&mut self) {
        self.io = CurrentIo::None;
        self.flags = Flags::empty();
        self.version = Version::HTTP_11;
        self.headers.clear();
        self.headers_vec.clear();
        self.extensions.get_mut().clear();
    }

    fn with_pool<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&MessagePool<Self>) -> R,
    {
        REQUEST_POOL.try_with(|p| f(p)).ok()
    }
}

impl RequestHead {
    /// Returns the request extensions.
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.extensions.borrow()
    }

    /// Returns mutable access to the request extensions.
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.extensions.borrow_mut()
    }

    /// Returns the request headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Returns mutable access to the request headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// Returns headers preserved in their original order and casing.
    pub fn headers_vec(&self) -> &[HeaderItem] {
        &self.headers_vec
    }

    #[inline]
    /// Sets the request connection behavior.
    ///
    /// Connection types are flags, and calling this method again does not clear
    /// a previously set type. When several are set, `Close` takes precedence
    /// over `KeepAlive`, which takes precedence over `Upgrade`.
    pub fn set_connection_type(&mut self, ctype: ConnectionType) {
        match ctype {
            ConnectionType::Close => self.flags.insert(Flags::CLOSE),
            ConnectionType::KeepAlive => self.flags.insert(Flags::KEEP_ALIVE),
            ConnectionType::Upgrade => self.flags.insert(Flags::UPGRADE),
        }
    }

    #[inline]
    /// Returns the request connection behavior.
    pub fn connection_type(&self) -> ConnectionType {
        if self.flags.contains(Flags::CLOSE) {
            ConnectionType::Close
        } else if self.flags.contains(Flags::KEEP_ALIVE) {
            ConnectionType::KeepAlive
        } else if self.flags.contains(Flags::UPGRADE) {
            ConnectionType::Upgrade
        } else if self.version < Version::HTTP_11 {
            ConnectionType::Close
        } else {
            ConnectionType::KeepAlive
        }
    }

    #[inline]
    /// Returns whether the request upgrades the connection.
    pub fn upgrade(&self) -> bool {
        self.flags.contains(Flags::UPGRADE)
    }

    #[inline]
    /// Returns whether the request contains `Expect: 100-continue`.
    pub fn expect(&self) -> bool {
        self.flags.contains(Flags::EXPECT)
    }

    #[inline]
    /// Returns whether chunked transfer encoding is allowed.
    pub fn chunked(&self) -> bool {
        !self.flags.contains(Flags::NO_CHUNKING)
    }

    #[inline]
    /// Enables or disables chunked transfer encoding.
    ///
    /// A streaming request body without chunked coding requires a `Content-Length` header.
    pub fn no_chunking(&mut self, val: bool) {
        if val {
            self.flags.insert(Flags::NO_CHUNKING);
        } else {
            self.flags.remove(Flags::NO_CHUNKING);
        }
    }

    #[inline]
    pub(crate) fn set_expect(&mut self) {
        self.flags.insert(Flags::EXPECT);
    }

    #[inline]
    pub(crate) fn set_upgrade(&mut self) {
        self.flags.insert(Flags::UPGRADE);
    }

    /// Returns the peer socket address.
    ///
    /// Peer address is actual socket address, if proxy is used in front of
    /// ntex http server, then peer address would be address of this proxy.
    #[inline]
    pub fn peer_addr(&self) -> Option<net::SocketAddr> {
        self.io.as_ref().and_then(|io| {
            io.query::<types::PeerAddr>()
                .get()
                .map(types::PeerAddr::into_inner)
        })
    }

    /// Takes ownership of the I/O stream and HTTP/1 codec for an upgrade.
    ///
    /// The handle is installed only after an upgrade is acknowledged through
    /// the HTTP/1 control service. This is a one-shot operation: subsequent
    /// calls return [`None`].
    pub fn take_io(&self) -> Option<(IoBoxed, Codec)> {
        self.io.take()
    }

    #[doc(hidden)]
    pub fn remove_io(&mut self) {
        self.io = CurrentIo::None;
    }
}

/// The metadata for an HTTP response.
#[derive(Debug)]
pub struct ResponseHead {
    /// HTTP protocol version.
    pub version: Version,
    /// Response status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Headers in their original order and with their original names.
    ///
    /// This collection is populated when decoding a response with
    /// [`HttpServiceConfig::set_headers_vec`](crate::http::HttpServiceConfig::set_headers_vec)
    /// enabled.
    pub headers_vec: Vec<HeaderItem>,
    /// Custom reason phrase, or `None` to use the status code's standard phrase.
    pub reason: Option<&'static str>,
    pub(crate) io: CurrentIo,
    pub(crate) extensions: RefCell<Extensions>,
    flags: Flags,
}

impl ResponseHead {
    /// Creates response metadata with the supplied status and HTTP version.
    #[inline]
    pub fn new(status: StatusCode, version: Version) -> ResponseHead {
        ResponseHead {
            status,
            version,
            headers: HeaderMap::with_capacity(12),
            headers_vec: Vec::default(),
            reason: None,
            flags: Flags::empty(),
            io: CurrentIo::None,
            extensions: RefCell::new(Extensions::new()),
        }
    }

    /// Returns the response extensions.
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.extensions.borrow()
    }

    /// Returns mutable access to the response extensions.
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.extensions.borrow_mut()
    }

    #[inline]
    /// Returns the response headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[inline]
    /// Returns mutable access to the response headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// Returns headers preserved in their original order and casing.
    pub fn headers_vec(&self) -> &[HeaderItem] {
        &self.headers_vec
    }

    #[inline]
    /// Sets the response connection behavior.
    ///
    /// Connection types are flags, and calling this method again does not clear
    /// a previously set type. When several are set, `Close` takes precedence
    /// over `KeepAlive`, which takes precedence over `Upgrade`.
    pub fn set_connection_type(&mut self, ctype: ConnectionType) {
        match ctype {
            ConnectionType::Close => self.flags.insert(Flags::CLOSE),
            ConnectionType::KeepAlive => self.flags.insert(Flags::KEEP_ALIVE),
            ConnectionType::Upgrade => self.flags.insert(Flags::UPGRADE),
        }
    }

    #[inline]
    /// Returns the response's connection behavior.
    pub fn connection_type(&self) -> ConnectionType {
        if self.flags.contains(Flags::CLOSE) {
            ConnectionType::Close
        } else if self.flags.contains(Flags::KEEP_ALIVE) {
            ConnectionType::KeepAlive
        } else if self.flags.contains(Flags::UPGRADE) {
            ConnectionType::Upgrade
        } else if self.version < Version::HTTP_11 {
            ConnectionType::Close
        } else {
            ConnectionType::KeepAlive
        }
    }

    #[inline]
    /// Returns whether the response keeps the connection open.
    pub fn keep_alive(&self) -> bool {
        self.connection_type() == ConnectionType::KeepAlive
    }

    #[inline]
    /// Returns whether the response upgrades the connection.
    pub fn upgrade(&self) -> bool {
        self.connection_type() == ConnectionType::Upgrade
    }

    /// Returns the custom or canonical reason phrase.
    ///
    /// The reason phrase is empty for a status code without a canonical reason.
    #[inline]
    pub fn reason(&self) -> &str {
        if let Some(reason) = self.reason {
            reason
        } else {
            self.status.canonical_reason().unwrap_or("")
        }
    }

    #[inline]
    pub(crate) fn ctype(&self) -> Option<ConnectionType> {
        if self.flags.contains(Flags::CLOSE) {
            Some(ConnectionType::Close)
        } else if self.flags.contains(Flags::KEEP_ALIVE) {
            Some(ConnectionType::KeepAlive)
        } else if self.flags.contains(Flags::UPGRADE) {
            Some(ConnectionType::Upgrade)
        } else {
            None
        }
    }

    #[inline]
    /// Returns whether HTTP/1 chunked transfer encoding is allowed.
    pub fn chunked(&self) -> bool {
        !self.flags.contains(Flags::NO_CHUNKING)
    }

    #[inline]
    /// Enables or disables HTTP/1 chunked transfer encoding.
    pub fn no_chunking(&mut self, val: bool) {
        if val {
            self.flags.insert(Flags::NO_CHUNKING);
        } else {
            self.flags.remove(Flags::NO_CHUNKING);
        }
    }
}

impl Default for ResponseHead {
    fn default() -> Self {
        Self::new(StatusCode::default(), Version::default())
    }
}

impl Head for ResponseHead {
    fn clear(&mut self) {
        self.reason = None;
        self.headers.clear();
        self.headers_vec.clear();
        self.io = CurrentIo::None;
        self.flags = Flags::empty();
        self.extensions.get_mut().clear();
    }

    fn with_pool<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&MessagePool<Self>) -> R,
    {
        RESPONSE_POOL.try_with(|p| f(p)).ok()
    }
}

#[derive(Debug)]
pub(crate) struct Message<T: Head> {
    head: Rc<T>,
}

impl<T: Head> Message<T> {
    /// Get new message from the pool of objects
    pub(crate) fn new() -> Self {
        let head = T::with_pool(MessagePool::get_head)
            .flatten()
            .unwrap_or_else(|| Rc::new(T::default()));
        Message { head }
    }
}

impl Message<ResponseHead> {
    /// Get new message from the pool of objects
    pub(crate) fn with_status(status: StatusCode) -> Self {
        let mut msg = Self::new();
        msg.status = status;
        msg
    }

    /// Copies the response head into a new message, extensions are not copied.
    pub(crate) fn copy(&self) -> Self {
        let mut msg = Self::with_status(self.status);
        msg.version = self.version;
        msg.headers = self.headers.clone();
        msg.headers_vec.clone_from(&self.headers_vec);
        msg.reason = self.reason;
        msg.io = self.io.clone();
        msg.flags = self.flags;
        msg
    }
}

impl<T: Head> Clone for Message<T> {
    fn clone(&self) -> Self {
        Self {
            head: self.head.clone(),
        }
    }
}

impl<T: Head> std::ops::Deref for Message<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.head.as_ref()
    }
}

impl<T: Head> std::ops::DerefMut for Message<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Rc::get_mut(&mut self.head).expect("Multiple copies exist")
    }
}

impl<T: Head> Drop for Message<T> {
    fn drop(&mut self) {
        if let Some(head) = Rc::get_mut(&mut self.head) {
            // clearing may drop other messages (e.g. stored in extensions),
            // so the pool must not be borrowed at this point
            head.clear();
            T::with_pool(|pool| {
                if let Ok(mut v) = pool.0.try_borrow_mut()
                    && v.len() < 128
                {
                    v.push(self.head.clone());
                }
            });
        }
    }
}

/// Request's objects pool
pub(crate) struct MessagePool<T: Head>(RefCell<Vec<Rc<T>>>);

thread_local!(static REQUEST_POOL: MessagePool<RequestHead> = MessagePool::<RequestHead>::new());
thread_local!(static RESPONSE_POOL: MessagePool<ResponseHead> = MessagePool::<ResponseHead>::new());

impl<T: Head> MessagePool<T> {
    fn new() -> MessagePool<T> {
        MessagePool(RefCell::new(Vec::with_capacity(256)))
    }

    /// Get cleared head from the pool
    #[inline]
    fn get_head(&self) -> Option<Rc<T>> {
        self.0.try_borrow_mut().ok()?.pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_message_drop() {
        let inner = Message::<RequestHead>::new();
        let outer = Message::<RequestHead>::new();
        outer.extensions_mut().insert(inner);
        let mut res = Message::<ResponseHead>::with_status(StatusCode::OK);
        res.extensions
            .get_mut()
            .insert(Message::<ResponseHead>::new());
        outer.extensions_mut().insert(res);
        drop(outer);

        let _ = Message::<RequestHead>::new();
        let _ = Message::<ResponseHead>::new();
    }

    #[test]
    fn message_drop_after_pool_destroyed() {
        struct Holder(Option<Message<RequestHead>>, Option<Message<ResponseHead>>);

        impl Drop for Holder {
            fn drop(&mut self) {
                drop(self.0.take());
                drop(self.1.take());
                drop(Message::<RequestHead>::new());
            }
        }

        thread_local!(static HOLDER: RefCell<Option<Holder>> = const { RefCell::new(None) });

        std::thread::spawn(|| {
            // register holder before the pools, so it is destroyed after them
            HOLDER.with(|_| ());
            let req = Message::<RequestHead>::new();
            let res = Message::<ResponseHead>::new();
            HOLDER.with(|h| *h.borrow_mut() = Some(Holder(Some(req), Some(res))));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn response_connection_type() {
        let mut res = Message::<ResponseHead>::with_status(StatusCode::OK);
        assert_eq!(res.connection_type(), ConnectionType::KeepAlive);
        res.version = Version::HTTP_10;
        assert_eq!(res.connection_type(), ConnectionType::Close);
        res.set_connection_type(ConnectionType::KeepAlive);
        assert_eq!(res.connection_type(), ConnectionType::KeepAlive);
    }

    #[test]
    fn message_clone() {
        let mut msg = Message::<RequestHead>::new();
        msg.method = Method::POST;
        let msg2 = msg.clone();
        assert_eq!(msg2.method, Method::POST);
        assert!(CurrentIo::None.take().is_none());
    }
}
