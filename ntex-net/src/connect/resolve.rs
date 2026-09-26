use std::{io, net};

use ntex_error::Error;
use ntex_rt::spawn_blocking;
use ntex_util::future::Either;

use super::{Address, Connect, ConnectError, message::parse};

/// Lookup ip addresses for provided host
pub(crate) async fn lookup<T: Address>(
    mut req: Connect<T>,
    tag: &str,
) -> Result<Connect<T>, Error<ConnectError>> {
    if req.addr.is_some() || req.req.addr().is_some() {
        Ok(req)
    } else if let Some(addr) = ip_literal(parse(req.host()).0, req.port()) {
        req.addr = Some(Either::Left(addr));
        Ok(req)
    } else {
        log::trace!("{tag}: DNS Resolver - resolving host {:?}", req.host());

        let host = (parse(req.host()).0.to_string(), req.port());

        let fut = spawn_blocking(move || net::ToSocketAddrs::to_socket_addrs(&host));
        match fut.await {
            Ok(Ok(ips)) => {
                let port = req.port();
                req = req.set_addrs(ips.rev().map(|mut ip| {
                    ip.set_port(port);
                    ip
                }));

                log::trace!(
                    "{}: DNS Resolver - host {:?} resolved to {:?}",
                    tag,
                    req.host(),
                    req.addrs()
                );

                if req.addr.is_none() {
                    Err(ConnectError::NoRecords.into())
                } else {
                    Ok(req)
                }
            }
            Ok(Err(e)) => {
                log::trace!(
                    "{}: DNS Resolver - failed to resolve host {:?} err: {}",
                    tag,
                    req.host(),
                    e
                );
                Err(ConnectError::Resolver(e).into())
            }
            Err(e) => {
                log::trace!(
                    "{}: DNS Resolver - failed to resolve host {:?} err: {}",
                    tag,
                    req.host(),
                    e
                );
                Err(ConnectError::Resolver(io::Error::other(e)).into())
            }
        }
    }
}

/// Parses an ip address, including an IPv6 address with a numeric zone id (`fe80::1%3`).
fn ip_literal(host: &str, port: u16) -> Option<net::SocketAddr> {
    if let Ok(ip) = host.parse() {
        return Some(net::SocketAddr::new(ip, port));
    }
    let (ip, zone) = host.split_once('%')?;
    let ip = ip.parse().ok()?;
    let scope_id = zone.parse().ok()?;
    Some(net::SocketAddrV6::new(ip, port, 0, scope_id).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_literals() {
        let addr = ip_literal("fe80::1%3", 80).unwrap();
        assert_eq!(addr, "[fe80::1%3]:80".parse().unwrap());
        let net::SocketAddr::V6(addr) = addr else {
            panic!("expected v6 address")
        };
        assert_eq!(addr.scope_id(), 3);
        assert_eq!(ip_literal("::1", 80), Some("[::1]:80".parse().unwrap()));
        assert_eq!(ip_literal("fe80::1%eth0", 80), None);
        assert_eq!(ip_literal("127.0.0.1%3", 80), None);
        assert_eq!(ip_literal("localhost", 80), None);
    }

    #[allow(clippy::clone_on_copy)]
    #[ntex::test]
    async fn resolver() {
        let res = lookup(Connect::new("www.rust-lang.org"), "").await;
        assert!(res.is_ok());

        let res = lookup(Connect::new("---11213"), "").await;
        assert!(res.is_err());

        let addr: net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let res = lookup(Connect::new("www.rust-lang.org").set_addrs(vec![addr]), "")
            .await
            .unwrap();
        let addrs: Vec<_> = res.addrs().collect();
        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&addr));
    }

    #[ntex::test]
    async fn resolver_ip_literals() {
        for (host, port, expected) in [
            ("127.0.0.1:8080", None, "127.0.0.1:8080"),
            ("[::1]:8080", None, "[::1]:8080"),
            ("[::1]", Some(9090), "[::1]:9090"),
            ("::1", Some(9090), "[::1]:9090"),
            ("fe80::1%3", Some(80), "[fe80::1%3]:80"),
            ("[fe80::1%3]:8080", None, "[fe80::1%3]:8080"),
        ] {
            let mut req = Connect::new(host);
            if let Some(port) = port {
                req = req.set_port(port);
            }
            let res = lookup(req, "").await.unwrap();
            let addrs: Vec<_> = res.addrs().collect();
            assert_eq!(addrs, vec![expected.parse().unwrap()], "{host}");
        }

        let uri = ntex_http::Uri::from_static("http://[::1]:8080/");
        let res = lookup(Connect::new(uri), "").await.unwrap();
        let addrs: Vec<_> = res.addrs().collect();
        assert_eq!(addrs, vec!["[::1]:8080".parse().unwrap()]);

        let res = lookup(Connect::new("localhost:8080"), "").await.unwrap();
        assert!(
            res.addrs()
                .all(|a| a.port() == 8080 && a.ip().is_loopback())
        );
    }
}
