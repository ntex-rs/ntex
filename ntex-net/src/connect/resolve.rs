use std::{fmt, io, marker, net};

use ntex_rt::spawn_blocking;
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::future::Either;

use super::{Address, Connect, ConnectError};

/// DNS Resolver Service
pub struct Resolver<T>(marker::PhantomData<T>);

impl<T> Resolver<T> {
    /// Create new resolver instance with custom configuration and options.
    pub fn new() -> Self {
        Resolver(marker::PhantomData)
    }
}

impl<T> Copy for Resolver<T> {}

impl<T: Address> Resolver<T> {
    /// Lookup ip addresses for provided host
    pub async fn lookup(&self, req: Connect<T>) -> Result<Connect<T>, ConnectError> {
        self.lookup_with_tag(req, "TCP-CLIENT").await
    }

    #[doc(hidden)]
    /// Lookup ip addresses for provided host
    pub async fn lookup_with_tag(
        &self,
        mut req: Connect<T>,
        tag: &'static str,
    ) -> Result<Connect<T>, ConnectError> {
        if req.addr.is_some() || req.req.addr().is_some() {
            Ok(req)
        } else if let Ok(ip) = req.host().parse() {
            req.addr = Some(Either::Left(net::SocketAddr::new(ip, req.port())));
            Ok(req)
        } else {
            log::trace!("{}: DNS Resolver - resolving host {:?}", tag, req.host());

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

impl<T> fmt::Debug for Resolver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver").finish()
    }
}

impl<T: Address, C> ServiceFactory<Connect<T>, C> for Resolver<T> {
    type Response = Connect<T>;
    type Error = ConnectError;
    type Service = Resolver<T>;
    type InitError = ();

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        Ok(self.clone())
    }
}

impl<T: Address> Service<Connect<T>> for Resolver<T> {
    type Response = Connect<T>;
    type Error = ConnectError;

    async fn call(
        &self,
        req: Connect<T>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Connect<T>, Self::Error> {
        self.lookup(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex_util::future::lazy;

    #[allow(clippy::clone_on_copy)]
    #[ntex::test]
    async fn resolver() {
        let resolver = Resolver::default().clone();
        assert!(format!("{:?}", resolver).contains("Resolver"));
        let srv = resolver.pipeline(()).await.unwrap().bind();
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
