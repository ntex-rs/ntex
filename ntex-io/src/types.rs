use std::{any, fmt, marker::PhantomData, net::SocketAddr};

pub struct PeerAddr(pub SocketAddr);

impl fmt::Debug for PeerAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
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

    pub fn get(&self) -> Option<&T> {
        if let Some(ref item) = self.item {
            item.downcast_ref()
        } else {
            None
        }
    }
}
