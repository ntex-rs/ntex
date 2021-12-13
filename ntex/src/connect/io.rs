use std::task::{Context, Poll};
use std::{future::Future, pin::Pin};

use crate::io::Io;
use crate::service::{Service, ServiceFactory};
use crate::util::{PoolId, PoolRef, Ready};

use super::service::ConnectServiceResponse;
use super::{Address, Connect, ConnectError, Connector};

pub struct IoConnector<T> {
    inner: Connector<T>,
    pool: PoolRef,
}

impl<T> IoConnector<T> {
    /// Construct new connect service with custom dns resolver
    pub fn new() -> Self {
        IoConnector {
            inner: Connector::new(),
            pool: PoolId::P0.pool_ref(),
        }
    }

    /// Set memory pool.
    ///
    /// Use specified memory pool for memory allocations. By default P0
    /// memory pool is used.
    pub fn memory_pool(mut self, id: PoolId) -> Self {
        self.pool = id.pool_ref();
        self
    }
}

impl<T: Address> IoConnector<T> {
    /// Resolve and connect to remote host
    pub fn connect<U>(&self, message: U) -> IoConnectServiceResponse<T>
    where
        Connect<T>: From<U>,
    {
        IoConnectServiceResponse {
            inner: self.inner.call(message.into()),
            pool: self.pool,
        }
    }
}

impl<T> Default for IoConnector<T> {
    fn default() -> Self {
        IoConnector::new()
    }
}

impl<T> Clone for IoConnector<T> {
    fn clone(&self) -> Self {
        IoConnector {
            inner: self.inner.clone(),
            pool: self.pool,
        }
    }
}

impl<T: Address> ServiceFactory for IoConnector<T> {
    type Request = Connect<T>;
    type Response = Io;
    type Error = ConnectError;
    type Config = ();
    type Service = IoConnector<T>;
    type InitError = ();
    type Future = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(self.clone())
    }
}

impl<T: Address> Service for IoConnector<T> {
    type Request = Connect<T>;
    type Response = Io;
    type Error = ConnectError;
    type Future = IoConnectServiceResponse<T>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: Connect<T>) -> Self::Future {
        self.connect(req)
    }
}

#[doc(hidden)]
pub struct IoConnectServiceResponse<T: Address> {
    inner: ConnectServiceResponse<T>,
    pool: PoolRef,
}

impl<T: Address> Future for IoConnectServiceResponse<T> {
    type Output = Result<Io, ConnectError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.inner).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(stream)) => {
                Poll::Ready(Ok(Io::with_memory_pool(stream, self.pool)))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[crate::rt_test]
    async fn test_connect() {
        let server = crate::server::test_server(|| {
            crate::service::fn_service(|_| async { Ok::<_, ()>(()) })
        });

        let srv = IoConnector::default();
        let result = srv.connect("").await;
        assert!(result.is_err());
        let result = srv.connect("localhost:99999").await;
        assert!(result.is_err());

        let srv = IoConnector::default();
        let result = srv.connect(format!("{}", server.addr())).await;
        assert!(result.is_ok());

        let msg = Connect::new(format!("{}", server.addr())).set_addrs(vec![
            format!("127.0.0.1:{}", server.addr().port() - 1)
                .parse()
                .unwrap(),
            server.addr(),
        ]);
        let result = crate::connect::connect(msg).await;
        assert!(result.is_ok());

        let msg = Connect::new(server.addr());
        let result = crate::connect::connect(msg).await;
        assert!(result.is_ok());
    }
}
