use std::{fmt, io, marker, net};

use ntex_rt::spawn_blocking;
use ntex_service::{Ctx, Service, ServiceFactory};
use ntex_util::future::{BoxFuture, Either, Ready};

use crate::{Address, Connect, ConnectError};

/// DNS Resolver Service
pub struct Resolver<T>(marker::PhantomData<T>);

impl<T> fmt::Debug for Resolver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver").finish()
    }
}

impl<T> Resolver<T> {
    /// Create new resolver instance with custom configuration and options.
    pub fn new() -> Self {
        Resolver(marker::PhantomData)
    }
}

impl<T: Address> Resolver<T> {
    /// Lookup ip addresses for provided host
    pub async fn lookup(&self, mut req: Connect<T>) -> Result<Connect<T>, ConnectError> {
        if req.addr.is_some() || req.req.addr().is_some() {
            Ok(req)
        } else if let Ok(ip) = req.host().parse() {
            req.addr = Some(Either::Left(net::SocketAddr::new(ip, req.port())));
            Ok(req)
        } else {
            trace!("DNS resolver: resolving host {:?}", req.host());

            let host = if req.host().contains(':') {
                req.host().to_string()
            } else {
                format!("{}:{}", req.host(), req.port())
            };

            let fut = spawn_blocking(move || net::ToSocketAddrs::to_socket_addrs(&host));
            match fut.await {
                Ok(Ok(ips)) => {
                    let port = req.port();
                    let req = req.set_addrs(ips.map(|mut ip| {
                        ip.set_port(port);
                        ip
                    }));

                    trace!(
                        "DNS resolver: host {:?} resolved to {:?}",
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
                    trace!(
                        "DNS resolver: failed to resolve host {:?} err: {}",
                        req.host(),
                        e
                    );
                    Err(ConnectError::Resolver(e))
                }
                Err(e) => {
                    trace!(
                        "DNS resolver: failed to resolve host {:?} err: {}",
                        req.host(),
                        e
                    );
                    Err(ConnectError::Resolver(io::Error::new(
                        io::ErrorKind::Other,
                        e,
                    )))
                }
            }
        }
    }
}

impl<T> Default for Resolver<T> {
    fn default() -> Resolver<T> {
        Resolver::new()
    }
}

impl<T> Clone for Resolver<T> {
    fn clone(&self) -> Self {
        Resolver(marker::PhantomData)
    }
}

impl<T: Address, C: 'static> ServiceFactory<Connect<T>, C> for Resolver<T> {
    type Response = Connect<T>;
    type Error = ConnectError;
    type Service = Resolver<T>;
    type InitError = ();
    type Future<'f> = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn create(&self, _: C) -> Self::Future<'_> {
        Ready::Ok(self.clone())
    }
}

impl<T: Address> Service<Connect<T>> for Resolver<T> {
    type Response = Connect<T>;
    type Error = ConnectError;
    type Future<'f> = BoxFuture<'f, Result<Connect<T>, Self::Error>>;

    #[inline]
    fn call<'a>(&'a self, req: Connect<T>, _: Ctx<'a, Self>) -> Self::Future<'_> {
        Box::pin(self.lookup(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex_util::future::lazy;

    #[ntex::test]
    async fn resolver() {
        let resolver = Resolver::default().clone();
        assert!(format!("{:?}", resolver).contains("Resolver"));
        let srv = resolver.container(()).await.unwrap();
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());

        let res = srv.call(Connect::new("www.rust-lang.org")).await;
        assert!(res.is_ok());

        let res = srv.call(Connect::new("---11213")).await;
        assert!(res.is_err());

        let addr: net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let res = srv
            .call(Connect::new("www.rust-lang.org").set_addrs(vec![addr]))
            .await
            .unwrap();
        let addrs: Vec<_> = res.addrs().collect();
        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&addr));
    }
}
