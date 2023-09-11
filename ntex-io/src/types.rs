//! Query related types
use std::{any, fmt, marker::PhantomData, net::SocketAddr};

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct PeerAddr(pub SocketAddr);

impl PeerAddr {
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
/// Http protocol definition
pub enum HttpProtocol {
    Http1,
    Http2,
    Unknown,
}

pub struct QueryItem<T> {
    item: Option<Box<dyn any::Any>>,
    _t: PhantomData<T>,
}

impl<T: any::Any> QueryItem<T> {
    pub(crate) fn new(item: Box<dyn any::Any>) -> Self {
        Self {
            item: Some(item),
            _t: PhantomData,
        }
    }

    pub(crate) fn empty() -> Self {
        Self {
            item: None,
            _t: PhantomData,
        }
    }

    pub fn get(&self) -> Option<T>
    where
        T: Copy,
    {
        self.item.as_ref().and_then(|v| v.downcast_ref().copied())
    }

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
