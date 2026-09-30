//! Values exposed through transport and filter queries.
use std::{any, fmt, marker::PhantomData, net::SocketAddr};

#[derive(Copy, Clone, PartialEq, Eq)]
/// Peer socket address returned by [`IoRef::query`](crate::IoRef::query).
pub struct PeerAddr(pub SocketAddr);

impl PeerAddr {
    /// Returns the contained socket address.
    pub fn into_inner(self) -> SocketAddr {
        self.0
    }
}

impl From<SocketAddr> for PeerAddr {
    fn from(addr: SocketAddr) -> Self {
        Self(addr)
    }
}

impl fmt::Debug for PeerAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
/// HTTP protocol selected for an I/O stream.
pub enum HttpProtocol {
    /// HTTP/1.x.
    Http1,
    /// HTTP/2.
    Http2,
    /// The protocol has not been identified or is not HTTP.
    Unknown,
}

/// Typed result of an [`IoRef::query`](crate::IoRef::query).
///
/// Query providers store values as `dyn Any`; this wrapper performs the
/// requested downcast without exposing the erased representation.
pub struct QueryItem<T> {
    item: Option<Box<dyn any::Any>>,
    _t: PhantomData<T>,
}

impl<T: any::Any> QueryItem<T> {
    pub(crate) fn new(item: Option<Box<dyn any::Any>>) -> Self {
        Self {
            item,
            _t: PhantomData,
        }
    }

    /// Copies the queried value out of this item.
    ///
    /// Returns `None` if no provider returned a value of type `T`.
    pub fn get(&self) -> Option<T>
    where
        T: Copy,
    {
        self.item.as_ref().and_then(|v| v.downcast_ref().copied())
    }

    /// Borrows the queried value.
    ///
    /// Returns `None` if no provider returned a value of type `T`.
    pub fn as_ref(&self) -> Option<&T> {
        if let Some(ref item) = self.item {
            item.downcast_ref()
        } else {
            None
        }
    }
}

impl<T: any::Any + fmt::Debug> fmt::Debug for QueryItem<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(v) = self.as_ref() {
            f.debug_tuple("QueryItem").field(v).finish()
        } else {
            f.debug_tuple("QueryItem").field(&None::<T>).finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_addr() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let peer = PeerAddr::from(addr);
        assert_eq!(peer, PeerAddr(addr));
        assert_eq!(format!("{peer:?}"), "127.0.0.1:8080");
        assert_eq!(peer.into_inner(), addr);
    }

    #[test]
    fn query_item() {
        let item = QueryItem::<u32>::new(Some(Box::new(10u32)));
        assert_eq!(item.get(), Some(10));
        assert_eq!(item.as_ref(), Some(&10));
        assert_eq!(format!("{item:?}"), "QueryItem(10)");

        // a value of another type is not returned
        let item = QueryItem::<u32>::new(Some(Box::new("str")));
        assert_eq!(item.get(), None);
        assert_eq!(item.as_ref(), None);

        let item = QueryItem::<u32>::new(None);
        assert_eq!(item.get(), None);
        assert_eq!(item.as_ref(), None);
        assert_eq!(format!("{item:?}"), "QueryItem(None)");
    }
}
