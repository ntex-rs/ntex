use std::{
    fmt, marker::PhantomData, net::SocketAddr, pin::Pin, rc::Rc, task::Context,
    task::Poll,
};

use futures::future::{ok, Either, Future, Ready};

use super::{default_resolver, Address, Connect, ConnectError, DnsResolver};
use crate::service::{Service, ServiceFactory};

/// DNS Resolver Service
pub struct Resolver<T> {
    resolver: Rc<DnsResolver>,
    _t: PhantomData<T>,
}

impl<T> fmt::Debug for Resolver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("resolver", &self.resolver)
            .finish()
    }
}

impl<T> Resolver<T> {
    /// Create new resolver instance with custom configuration and options.
    pub fn new(resolver: DnsResolver) -> Self {
        Resolver {
            resolver: Rc::new(resolver),
            _t: PhantomData,
        }
    }
}

impl<T: Address> Resolver<T> {
    /// Lookup ip addresses for provided host
    pub fn lookup(
        &self,
        mut req: Connect<T>,
    ) -> impl Future<Output = Result<Connect<T>, ConnectError>> {
        if req.addr.is_some() || req.req.addr().is_some() {
            Either::Right(ok(req))
        } else if let Ok(ip) = req.host().parse() {
            req.addr = Some(either::Either::Left(SocketAddr::new(ip, req.port())));
            Either::Right(ok(req))
        } else {
            trace!("DNS resolver: resolving host {:?}", req.host());
            let resolver = self.resolver.clone();

            Either::Left(async move {
                let fut = if let Some(host) = req.host().splitn(2, ':').next() {
                    resolver.lookup_ip(host)
                } else {
                    resolver.lookup_ip(req.host())
                };

                match fut.await {
                    Ok(ips) => {
                        let port = req.port();
                        let req = req
                            .set_addrs(ips.iter().map(|ip| SocketAddr::new(ip, port)));

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
                    Err(e) => {
                        trace!(
                            "DNS resolver: failed to resolve host {:?} err: {}",
                            req.host(),
                            e
                        );
                        Err(e.into())
                    }
                }
            })
        }
    }
}

impl<T> Default for Resolver<T> {
    fn default() -> Resolver<T> {
        Resolver {
            resolver: Rc::new(default_resolver()),
            _t: PhantomData,
        }
    }
}

impl<T> Clone for Resolver<T> {
    fn clone(&self) -> Self {
        Resolver {
            resolver: self.resolver.clone(),
            _t: PhantomData,
        }
    }
}

impl<T: Address> ServiceFactory for Resolver<T> {
    type Request = Connect<T>;
    type Response = Connect<T>;
    type Error = ConnectError;
    type Config = ();
    type Service = Resolver<T>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ok(self.clone())
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
    use futures::future::lazy;

    use super::*;

    #[crate::rt_test]
    async fn resolver() {
        let resolver = Resolver::new(DnsResolver::tokio_from_system_conf().unwrap());
        assert!(format!("{:?}", resolver).contains("Resolver"));
        let srv = resolver.new_service(()).await.unwrap();
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());

        let res = srv.call(Connect::new("www.rust-lang.org")).await;
        assert!(res.is_ok());

        let res = srv.call(Connect::new("---11213")).await;
        assert!(res.is_err());

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let res = srv
            .call(Connect::new("www.rust-lang.org").set_addrs(vec![addr]))
            .await
            .unwrap();
        let addrs: Vec<_> = res.addrs().collect();
        assert_eq!(addrs.len(), 1);
        assert!(addrs.contains(&addr));
    }
}
