use std::{fmt, future::Future, io, marker, net, pin::Pin, task::Context, task::Poll};

use super::{Address, Connect, ConnectError};
use crate::service::{Service, ServiceFactory};
use crate::util::{Either, Ready};

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
    pub fn lookup(
        &self,
        mut req: Connect<T>,
    ) -> impl Future<Output = Result<Connect<T>, ConnectError>> {
        if req.addr.is_some() || req.req.addr().is_some() {
            Either::Right(Ready::Ok(req))
        } else if let Ok(ip) = req.host().parse() {
            req.addr = Some(Either::Left(net::SocketAddr::new(ip, req.port())));
            Either::Right(Ready::Ok(req))
        } else {
            trace!("DNS resolver: resolving host {:?}", req.host());

            Either::Left(async move {
                let host = if req.host().contains(':') {
                    req.host().to_string()
                } else {
                    format!("{}:{}", req.host(), req.port())
                };

                let fut = crate::rt::task::spawn_blocking(move || {
                    net::ToSocketAddrs::to_socket_addrs(&host)
                });

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
            })
        }
    }
}

impl<T> Default for Resolver<T> {
    fn default() -> Resolver<T> {
        Resolver(marker::PhantomData)
    }
}

impl<T> Clone for Resolver<T> {
    fn clone(&self) -> Self {
        Resolver(marker::PhantomData)
    }
}

impl<T: Address> ServiceFactory for Resolver<T> {
    type Request = Connect<T>;
    type Response = Connect<T>;
    type Error = ConnectError;
    type Config = ();
    type Service = Resolver<T>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(self.clone())
    }
}

impl<T: Address> Service for Resolver<T> {
    type Request = Connect<T>;
    type Response = Connect<T>;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Connect<T>, Self::Error>>>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: Connect<T>) -> Self::Future {
        Box::pin(self.lookup(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::lazy;

    #[crate::rt_test]
    async fn resolver() {
        let resolver = Resolver::new();
        assert!(format!("{:?}", resolver).contains("Resolver"));
        let srv = resolver.new_service(()).await.unwrap();
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
