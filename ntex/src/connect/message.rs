use std::collections::{vec_deque, VecDeque};
use std::fmt;
use std::iter::{FromIterator, FusedIterator};
use std::net::SocketAddr;

use crate::util::Either;

/// Connect request
pub trait Address: Unpin + 'static {
    /// Host name of the request
    fn host(&self) -> &str;

    /// Port of the request
    fn port(&self) -> Option<u16>;

    /// SocketAddr of the address
    fn addr(&self) -> Option<SocketAddr> {
        None
    }
}

impl Address for String {
    fn host(&self) -> &str {
        &self
    }

    fn port(&self) -> Option<u16> {
        None
    }
}

impl Address for &'static str {
    fn host(&self) -> &str {
        self
    }

    fn port(&self) -> Option<u16> {
        None
    }
}

impl Address for SocketAddr {
    fn host(&self) -> &str {
        ""
    }

    fn port(&self) -> Option<u16> {
        None
    }

    fn addr(&self) -> Option<SocketAddr> {
        Some(*self)
    }
}

/// Connect request
#[derive(Eq, PartialEq, Debug, Hash)]
pub struct Connect<T> {
    pub(super) req: T,
    pub(super) port: u16,
    pub(super) addr: Option<Either<SocketAddr, VecDeque<SocketAddr>>>,
}

impl<T: Address> Connect<T> {
    /// Create `Connect` instance by spliting the string by ':' and convert the second part to u16
    pub fn new(req: T) -> Connect<T> {
        let (_, port) = parse(req.host());
        Connect {
            req,
            port: port.unwrap_or(0),
            addr: None,
        }
    }

    /// Create new `Connect` instance from host and address. Connector skips name resolution stage for such connect messages.
    pub fn with(req: T, addr: SocketAddr) -> Connect<T> {
        Connect {
            req,
            port: 0,
            addr: Some(Either::Left(addr)),
        }
    }

    /// Use port if address does not provide one.
    ///
    /// By default it set to 0
    pub fn set_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Use address.
    pub fn set_addr(mut self, addr: Option<SocketAddr>) -> Self {
        if let Some(addr) = addr {
            self.addr = Some(Either::Left(addr));
        }
        self
    }

    /// Use addresses.
    pub fn set_addrs<I>(mut self, addrs: I) -> Self
    where
        I: IntoIterator<Item = SocketAddr>,
    {
        let mut addrs = VecDeque::from_iter(addrs);
        self.addr = if addrs.len() < 2 {
            addrs.pop_front().map(Either::Left)
        } else {
            Some(Either::Right(addrs))
        };
        self
    }

    /// Host name
    pub fn host(&self) -> &str {
        self.req.host()
    }

    /// Port of the request
    pub fn port(&self) -> u16 {
        self.req.port().unwrap_or(self.port)
    }

    /// Preresolved addresses of the request.
    pub fn addrs(&self) -> ConnectAddrsIter<'_> {
        if let Some(addr) = self.req.addr() {
            ConnectAddrsIter {
                inner: Either::Left(Some(addr)),
            }
        } else {
            let inner = match self.addr {
                None => Either::Left(None),
                Some(Either::Left(addr)) => Either::Left(Some(addr)),
                Some(Either::Right(ref addrs)) => Either::Right(addrs.iter()),
            };

            ConnectAddrsIter { inner }
        }
    }

    /// Takes preresolved addresses of the request.
    pub fn take_addrs(&mut self) -> ConnectTakeAddrsIter {
        if let Some(addr) = self.req.addr() {
            ConnectTakeAddrsIter {
                inner: Either::Left(Some(addr)),
            }
        } else {
            let inner = match self.addr.take() {
                None => Either::Left(None),
                Some(Either::Left(addr)) => Either::Left(Some(addr)),
                Some(Either::Right(addrs)) => Either::Right(addrs.into_iter()),
            };

            ConnectTakeAddrsIter { inner }
        }
    }

    /// Return reference to inner type
    pub fn get_ref(&self) -> &T {
        &self.req
    }
}

impl<T: Address> From<T> for Connect<T> {
    fn from(addr: T) -> Self {
        Connect::new(addr)
    }
}

impl<T: Address> fmt::Display for Connect<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host(), self.port())
    }
}

/// Iterator over addresses in a [`Connect`](struct.Connect.html) request.
#[derive(Clone)]
pub struct ConnectAddrsIter<'a> {
    inner: Either<Option<SocketAddr>, vec_deque::Iter<'a, SocketAddr>>,
}

impl Iterator for ConnectAddrsIter<'_> {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner {
            Either::Left(ref mut opt) => opt.take(),
            Either::Right(ref mut iter) => iter.next().copied(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self.inner {
            Either::Left(Some(_)) => (1, Some(1)),
            Either::Left(None) => (0, Some(0)),
            Either::Right(ref iter) => iter.size_hint(),
        }
    }
}

impl fmt::Debug for ConnectAddrsIter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

impl ExactSizeIterator for ConnectAddrsIter<'_> {}

impl FusedIterator for ConnectAddrsIter<'_> {}

/// Owned iterator over addresses in a [`Connect`](struct.Connect.html) request.
#[derive(Debug)]
pub struct ConnectTakeAddrsIter {
    inner: Either<Option<SocketAddr>, vec_deque::IntoIter<SocketAddr>>,
}

impl Iterator for ConnectTakeAddrsIter {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner {
            Either::Left(ref mut opt) => opt.take(),
            Either::Right(ref mut iter) => iter.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self.inner {
            Either::Left(Some(_)) => (1, Some(1)),
            Either::Left(None) => (0, Some(0)),
            Either::Right(ref iter) => iter.size_hint(),
        }
    }
}

impl ExactSizeIterator for ConnectTakeAddrsIter {}

impl FusedIterator for ConnectTakeAddrsIter {}

fn parse(host: &str) -> (&str, Option<u16>) {
    let mut parts_iter = host.splitn(2, ':');
    if let Some(host) = parts_iter.next() {
        let port_str = parts_iter.next().unwrap_or("");
        if let Ok(port) = port_str.parse::<u16>() {
            (host, Some(port))
        } else {
            (host, None)
        }
    } else {
        (host, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address() {
        assert_eq!("test".host(), "test");
        assert_eq!("test".port(), None);

        let s = "test".to_string();
        assert_eq!(s.host(), "test");
        assert_eq!(s.port(), None);
    }

    #[test]
    fn connect() {
        let mut connect = Connect::new("www.rust-lang.org");
        assert_eq!(connect.host(), "www.rust-lang.org");
        assert_eq!(connect.port(), 0);
        assert_eq!(*connect.get_ref(), "www.rust-lang.org");
        connect = connect.set_port(80);
        assert_eq!(connect.port(), 80);

        let addrs: Vec<_> = connect.addrs().collect();
        assert!(addrs.is_empty());

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        connect = connect.set_addrs(vec![addr]);
        let addrs: Vec<_> = connect.take_addrs().collect();
        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&addr));

        let addr2: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        connect = connect.set_addrs(vec![addr, addr2]);
        let addrs: Vec<_> = connect.addrs().collect();
        assert_eq!(addrs.len(), 2);
        assert!(addrs.contains(&addr));
        assert!(addrs.contains(&addr2));

        let addrs: Vec<_> = connect.take_addrs().collect();
        assert_eq!(addrs.len(), 2);
        assert!(addrs.contains(&addr));
        assert!(addrs.contains(&addr2));

        let addrs: Vec<_> = connect.addrs().collect();
        assert!(addrs.is_empty());

        connect = connect.set_addrs(vec![addr]);
        assert_eq!(format!("{}", connect), "www.rust-lang.org:80");

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mut connect = Connect::new(addr);
        assert_eq!(connect.host(), "");
        assert_eq!(connect.port(), 0);
        let addrs: Vec<_> = connect.addrs().collect();
        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&addr));
        let addrs: Vec<_> = connect.take_addrs().collect();
        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&addr));
    }
}
