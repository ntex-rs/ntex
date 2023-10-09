use std::{cell::Ref, cell::RefCell, cell::RefMut, net, rc::Rc};

use bitflags::bitflags;

use crate::http::header::HeaderMap;
use crate::http::{h1::Codec, Method, StatusCode, Uri, Version};
use crate::io::{types, IoBoxed, IoRef};
use crate::util::Extensions;

/// Represents various types of connection
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

pub(crate) trait Head: Default + 'static {
    fn clear(&mut self);

    fn with_pool<F, R>(f: F) -> R
    where
        F: FnOnce(&MessagePool<Self>) -> R;
}

#[derive(Clone, Debug)]
pub(crate) enum CurrentIo {
    Ref(IoRef),
    Io(Rc<(IoRef, RefCell<Option<Box<(IoBoxed, Codec)>>>)>),
    None,
}

impl CurrentIo {
    pub(crate) fn as_ref(&self) -> Option<&IoRef> {
        match self {
            CurrentIo::Ref(ref io) => Some(io),
            CurrentIo::Io(ref io) => Some(&io.0),
            CurrentIo::None => None,
        }
    }
}

#[derive(Debug)]
pub struct RequestHead {
    pub uri: Uri,
    pub method: Method,
    pub version: Version,
    pub headers: HeaderMap,
    pub extensions: RefCell<Extensions>,
    pub(crate) io: CurrentIo,
    pub(crate) flags: Flags,
}

impl Default for RequestHead {
    fn default() -> RequestHead {
        RequestHead {
            io: CurrentIo::None,
            uri: Uri::default(),
            method: Method::default(),
            version: Version::HTTP_11,
            headers: HeaderMap::with_capacity(16),
            flags: Flags::empty(),
            extensions: RefCell::new(Extensions::new()),
        }
    }
}

impl Head for RequestHead {
    fn clear(&mut self) {
        self.io = CurrentIo::None;
        self.flags = Flags::empty();
        self.headers.clear();
        self.extensions.get_mut().clear();
    }

    fn with_pool<F, R>(f: F) -> R
    where
        F: FnOnce(&MessagePool<Self>) -> R,
    {
        REQUEST_POOL.with(|p| f(p))
    }
}

impl RequestHead {
    /// Message extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.extensions.borrow()
    }

    /// Mutable reference to a the message's extensions
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.extensions.borrow_mut()
    }

    /// Read the message headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Mutable reference to the message headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    #[inline]
    /// Set connection type of the message
    pub fn set_connection_type(&mut self, ctype: ConnectionType) {
        match ctype {
            ConnectionType::Close => self.flags.insert(Flags::CLOSE),
            ConnectionType::KeepAlive => self.flags.insert(Flags::KEEP_ALIVE),
            ConnectionType::Upgrade => self.flags.insert(Flags::UPGRADE),
        }
    }

    #[inline]
    /// Connection type
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
    /// Connection upgrade status
    pub fn upgrade(&self) -> bool {
        self.flags.contains(Flags::UPGRADE)
    }

    #[inline]
    /// Request contains `EXPECT` header
    pub fn expect(&self) -> bool {
        self.flags.contains(Flags::EXPECT)
    }

    #[inline]
    /// Get response body chunking state
    pub fn chunked(&self) -> bool {
        !self.flags.contains(Flags::NO_CHUNKING)
    }

    #[inline]
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

    /// Peer socket address
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

    /// Take io and codec for current request
    ///
    /// This objects are set only for upgrade requests
    pub fn take_io(&self) -> Option<Box<(IoBoxed, Codec)>> {
        match self.io {
            CurrentIo::Io(ref inner) => inner.1.borrow_mut().take(),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum RequestHeadType {
    Owned(RequestHead),
    Rc(Rc<RequestHead>, Option<HeaderMap>),
}

impl RequestHeadType {
    pub fn extra_headers(&self) -> Option<&HeaderMap> {
        match self {
            RequestHeadType::Owned(_) => None,
            RequestHeadType::Rc(_, headers) => headers.as_ref(),
        }
    }
}

impl AsRef<RequestHead> for RequestHeadType {
    fn as_ref(&self) -> &RequestHead {
        match self {
            RequestHeadType::Owned(head) => head,
            RequestHeadType::Rc(head, _) => head.as_ref(),
        }
    }
}

impl From<RequestHead> for RequestHeadType {
    fn from(head: RequestHead) -> Self {
        RequestHeadType::Owned(head)
    }
}

#[derive(Debug)]
pub struct ResponseHead {
    pub version: Version,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub reason: Option<&'static str>,
    pub(crate) io: CurrentIo,
    pub(crate) extensions: RefCell<Extensions>,
    flags: Flags,
}

impl ResponseHead {
    /// Create new instance of `ResponseHead` type
    #[inline]
    pub fn new(status: StatusCode) -> ResponseHead {
        ResponseHead {
            status,
            version: Version::default(),
            headers: HeaderMap::with_capacity(12),
            reason: None,
            flags: Flags::empty(),
            io: CurrentIo::None,
            extensions: RefCell::new(Extensions::new()),
        }
    }

    /// Message extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.extensions.borrow()
    }

    /// Mutable reference to a the message's extensions
    #[inline]
    pub fn extensions_mut(&self) -> RefMut<'_, Extensions> {
        self.extensions.borrow_mut()
    }

    #[inline]
    /// Read the message headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[inline]
    /// Mutable reference to the message headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    #[inline]
    /// Set connection type of the message
    pub fn set_connection_type(&mut self, ctype: ConnectionType) {
        match ctype {
            ConnectionType::Close => self.flags.insert(Flags::CLOSE),
            ConnectionType::KeepAlive => self.flags.insert(Flags::KEEP_ALIVE),
            ConnectionType::Upgrade => self.flags.insert(Flags::UPGRADE),
        }
    }

    #[inline]
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
    /// Check if keep-alive is enabled
    pub fn keep_alive(&self) -> bool {
        self.connection_type() == ConnectionType::KeepAlive
    }

    #[inline]
    /// Check upgrade status of this message
    pub fn upgrade(&self) -> bool {
        self.connection_type() == ConnectionType::Upgrade
    }

    /// Get custom reason for the response
    #[inline]
    pub fn reason(&self) -> &str {
        if let Some(reason) = self.reason {
            reason
        } else {
            self.status
                .canonical_reason()
                .unwrap_or("<unknown status code>")
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
    /// Get response body chunking state
    pub fn chunked(&self) -> bool {
        !self.flags.contains(Flags::NO_CHUNKING)
    }

    #[inline]
    /// Set no chunking for payload
    pub fn no_chunking(&mut self, val: bool) {
        if val {
            self.flags.insert(Flags::NO_CHUNKING);
        } else {
            self.flags.remove(Flags::NO_CHUNKING);
        }
    }

    pub(crate) fn set_io(&mut self, head: &RequestHead) {
        self.io = head.io.clone();
    }

    pub(crate) fn take_io(&self) -> Option<Box<(IoBoxed, Codec)>> {
        match self.io {
            CurrentIo::Io(ref inner) => inner.1.borrow_mut().take(),
            _ => None,
        }
    }
}

impl Default for ResponseHead {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl Head for ResponseHead {
    fn clear(&mut self) {
        self.reason = None;
        self.headers.clear();
        self.io = CurrentIo::None;
        self.flags = Flags::empty();
    }

    fn with_pool<F, R>(f: F) -> R
    where
        F: FnOnce(&MessagePool<Self>) -> R,
    {
        RESPONSE_POOL.with(|p| f(p))
    }
}

#[derive(Debug)]
pub(crate) struct Message<T: Head> {
    head: Rc<T>,
}

impl<T: Head> Message<T> {
    /// Get new message from the pool of objects
    pub(crate) fn new() -> Self {
        T::with_pool(|p| p.get_message())
    }
}

impl Message<ResponseHead> {
    /// Get new message from the pool of objects
    pub(crate) fn with_status(status: StatusCode) -> Self {
        let mut msg = RESPONSE_POOL.with(|p| p.get_message());
        msg.status = status;
        msg
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
        T::with_pool(|p| p.release(self.head.clone()));
    }
}

/// Request's objects pool
pub(crate) struct MessagePool<T: Head>(RefCell<Vec<Rc<T>>>);

thread_local!(static REQUEST_POOL: MessagePool<RequestHead> = MessagePool::<RequestHead>::new());
thread_local!(static RESPONSE_POOL: MessagePool<ResponseHead> = MessagePool::<ResponseHead>::new());

impl<T: Head> MessagePool<T> {
    fn new() -> MessagePool<T> {
        MessagePool(RefCell::new(Vec::with_capacity(128)))
    }

    /// Get message from the pool
    #[inline]
    fn get_message(&self) -> Message<T> {
        if let Some(mut msg) = self.0.borrow_mut().pop() {
            if let Some(r) = Rc::get_mut(&mut msg) {
                r.clear();
            }
            Message { head: msg }
        } else {
            Message {
                head: Rc::new(T::default()),
            }
        }
    }

    #[inline]
    /// Release request instance
    fn release(&self, msg: Rc<T>) {
        let v = &mut self.0.borrow_mut();
        if v.len() < 128 {
            v.push(msg);
        }
    }
}
