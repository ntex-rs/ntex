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
    } else if let Ok(ip) = parse(req.host()).0.parse() {
        req.addr = Some(Either::Left(net::SocketAddr::new(ip, req.port())));
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

#[cfg(test)]
mod tests {
    use super::*;

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
