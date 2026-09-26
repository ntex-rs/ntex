use std::collections::{VecDeque, vec_deque};
use std::{fmt, iter::FusedIterator, net::SocketAddr};

use ntex_bytes::ByteString;
use ntex_util::future::Either;

/// Address information required by [`Connect`].
pub trait Address: Unpin + 'static {
    /// Returns the host name.
    fn host(&self) -> &str;

    /// Returns an explicitly configured port, if available.
    fn port(&self) -> Option<u16>;

    /// Returns a pre-resolved socket address, if available.
    fn addr(&self) -> Option<SocketAddr> {
        None
    }
}

impl Address for String {
    fn host(&self) -> &str {
        self
    }

    fn port(&self) -> Option<u16> {
        None
    }
}

impl Address for ByteString {
    fn host(&self) -> &str {
        self
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
    fn host(&self) -> &'static str {
        ""
    }

    fn port(&self) -> Option<u16> {
        None
    }

    fn addr(&self) -> Option<SocketAddr> {
        Some(*self)
    }
}

/// Request to resolve and connect to a remote address.
#[derive(Eq, PartialEq, Debug, Hash)]
pub struct Connect<T> {
    pub(super) req: T,
    pub(super) port: u16,
    pub(super) addr: Option<Either<SocketAddr, VecDeque<SocketAddr>>>,
}

impl<T: Address> Connect<T> {
    /// Creates a connection request and derives a port from the host when present.
    #[must_use]
    pub fn new(req: T) -> Connect<T> {
        let (_, port) = parse(req.host());
        Connect {
            req,
            port: port.unwrap_or(0),
            addr: None,
        }
    }

    /// Creates a request with a pre-resolved socket address.
    ///
    /// The connector skips DNS resolution for this request.
    #[must_use]
    pub fn with(req: T, addr: SocketAddr) -> Connect<T> {
        Connect {
            req,
            port: 0,
            addr: Some(Either::Left(addr)),
        }
    }

    /// Sets the port used when [`Address::port()`] does not provide one.
    ///
    /// This replaces the port parsed from a `host:port` host by
    /// [`new()`](Self::new), which is zero if the host has none.
    #[must_use]
    pub fn set_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Sets one pre-resolved socket address.
    #[must_use]
    pub fn set_addr(mut self, addr: Option<SocketAddr>) -> Self {
        if let Some(addr) = addr {
            self.addr = Some(Either::Left(addr));
        }
        self
    }

    /// Sets multiple pre-resolved socket addresses.
    #[must_use]
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

    /// Returns the request host name.
    pub fn host(&self) -> &str {
        self.req.host()
    }

    /// Returns the port from [`Address::port()`], or the one parsed from the
    /// host or set by [`set_port()`](Self::set_port).
    pub fn port(&self) -> u16 {
        self.req.port().unwrap_or(self.port)
    }

    /// Iterates over the request's pre-resolved addresses.
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

    /// Removes and returns the request's pre-resolved addresses.
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

    /// Returns the original address value.
    pub fn get_ref(&self) -> &T {
        &self.req
    }

    /// Maps the original address value while preserving port and resolved addresses.
    pub fn map_addr<F, R>(self, f: F) -> Connect<R>
    where
        F: FnOnce(T) -> R,
    {
        let req = f(self.req);

        Connect {
            req,
            port: self.port,
            addr: self.addr,
        }
    }
}

impl<T: Clone> Clone for Connect<T> {
    fn clone(&self) -> Self {
        Connect {
            req: self.req.clone(),
            port: self.port,
            addr: self.addr.clone(),
        }
    }
}

impl<T: Address> From<T> for Connect<T> {
    fn from(addr: T) -> Self {
        Connect::new(addr)
    }
}

impl<T: Address> fmt::Display for Connect<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (host, _) = parse(self.host());
        if host.contains(':') {
            write!(f, "[{host}]:{}", self.port())
        } else {
            write!(f, "{host}:{}", self.port())
        }
    }
}

/// Iterator over addresses in a [`Connect`] request.
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

/// Owning iterator over addresses removed from a [`Connect`] request.
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

/// Splits `host`, `host:port`, `[v6]`, `[v6]:port` or bare `v6` into host and port.
///
/// Brackets are stripped from IPv6 hosts.
pub(super) fn parse(host: &str) -> (&str, Option<u16>) {
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((ip, tail)) => (ip, tail.strip_prefix(':')),
            None => (host, None),
        }
    } else {
        match host.split_once(':') {
            Some((name, port)) if !port.contains(':') => (name, Some(port)),
            _ => (host, None),
        }
    };
    (name, port.and_then(|p| p.parse::<u16>().ok()))
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

        let s = ByteString::from("test");
        assert_eq!(s.host(), "test");
        assert_eq!(s.port(), None);
    }

    #[test]
    fn parse_host() {
        assert_eq!(parse("example.com"), ("example.com", None));
        assert_eq!(parse("example.com:443"), ("example.com", Some(443)));
        assert_eq!(parse("example.com:bad"), ("example.com", None));
        assert_eq!(parse("127.0.0.1:8080"), ("127.0.0.1", Some(8080)));
        assert_eq!(parse("[::1]"), ("::1", None));
        assert_eq!(parse("[::1]:443"), ("::1", Some(443)));
        assert_eq!(parse("[::1]443"), ("::1", None));
        assert_eq!(parse("::1"), ("::1", None));
        assert_eq!(parse("2001:db8::1"), ("2001:db8::1", None));
        assert_eq!(parse("[::1"), ("[::1", None));
        assert_eq!(parse(""), ("", None));

        assert_eq!(Connect::new("[::1]:8080").port(), 8080);
        assert_eq!(Connect::new("::1").set_port(80).port(), 80);
    }

    #[test]
    fn display() {
        assert_eq!(
            Connect::new("example.com:443").to_string(),
            "example.com:443"
        );
        assert_eq!(
            Connect::new("example.com").set_port(80).to_string(),
            "example.com:80"
        );
        assert_eq!(Connect::new("[::1]:8080").to_string(), "[::1]:8080");
        assert_eq!(Connect::new("::1").set_port(80).to_string(), "[::1]:80");
        assert_eq!(
            Connect::new("fe80::1%3").set_port(80).to_string(),
            "[fe80::1%3]:80"
        );
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn connect() {
        let mut connect = Connect::new("www.rust-lang.org");
        assert_eq!(connect.host(), "www.rust-lang.org");
        assert_eq!(connect.port(), 0);
        assert_eq!(*connect.get_ref(), "www.rust-lang.org");
        connect = connect.set_port(80);
        assert_eq!(connect.port(), 80);
        let addrs = connect.addrs().clone();
        assert_eq!(format!("{addrs:?}"), "[]");
        assert!(connect.addrs().next().is_none());
        assert!(format!("{:?}", connect.clone()).contains("Connect"));

        let c = connect.clone().map_addr(|_| "www.rust-lang.org:80");
        assert_eq!(c.host(), "www.rust-lang.org:80");
        assert_eq!(c.port(), 80);
        let addrs = c.addrs().clone();
        assert_eq!(format!("{addrs:?}"), "[]");
        assert!(c.addrs().next().is_none());

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        connect = connect.set_addrs(vec![addr]);
        let addrs = connect.addrs().clone();
        assert_eq!(format!("{addrs:?}"), "[127.0.0.1:8080]");
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
        assert!(connect.addrs().next().is_none());

        connect = connect.set_addrs(vec![addr]);
        assert_eq!(format!("{connect}"), "www.rust-lang.org:80");

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
