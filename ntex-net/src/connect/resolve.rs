use std::{io, net};

use ntex_rt::spawn_blocking;
use ntex_util::future::Either;

use super::{Address, Connect, ConnectError};

/// Lookup ip addresses for provided host
pub(crate) async fn lookup<T: Address>(
    mut req: Connect<T>,
    tag: &str,
) -> Result<Connect<T>, ConnectError> {
    if req.addr.is_some() || req.req.addr().is_some() {
        Ok(req)
    } else if let Ok(ip) = req.host().parse() {
        req.addr = Some(Either::Left(net::SocketAddr::new(ip, req.port())));
        Ok(req)
    } else {
        log::trace!("{tag}: DNS Resolver - resolving host {:?}", req.host());

        let host = if req.host().contains(':') {
            req.host().to_string()
        } else {
            format!("{}:{}", req.host(), req.port())
        };

        let fut = spawn_blocking(move || net::ToSocketAddrs::to_socket_addrs(&host));
        match fut.await {
            Ok(Ok(ips)) => {
                let port = req.port();
                req = req.set_addrs(ips.map(|mut ip| {
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
                    Err(ConnectError::NoRecords)
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
                Err(ConnectError::Resolver(e))
            }
            Err(e) => {
                log::trace!(
                    "{}: DNS Resolver - failed to resolve host {:?} err: {}",
                    tag,
                    req.host(),
                    e
                );
                Err(ConnectError::Resolver(io::Error::other(e)))
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
}
