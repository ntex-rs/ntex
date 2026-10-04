use crate::error::{Error, ErrorMapping, with_service};
use crate::{Ctx, Service, SharedCfg, util::join};

use super::connection::Connection;
use super::error::{ClientError, ConnectError};
use super::{Connect, pool::ConnectionPool};

#[derive(Debug)]
/// Manages http client network connectivity.
pub(super) struct Connector {
    pub(super) tcp_pool: ConnectionPool,
    pub(super) ssl_pool: Option<ConnectionPool>,
}

impl Service<SharedCfg, Connect> for Connector {
    type Res = Connection;
    type Error = Error<ClientError>;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, SharedCfg>) -> Result<(), Self::Error> {
        if let Some(ref ssl_pool) = self.ssl_pool {
            let (r1, r2) = join(ctx.ready(&self.tcp_pool), ctx.ready(ssl_pool)).await;
            r1.into_error()?;
            r2.into_error()
        } else {
            ctx.ready(&self.tcp_pool).await.into_error()
        }
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, SharedCfg>) {
        ctx.shutdown(&self.tcp_pool).await;
        if let Some(ref ssl_pool) = self.ssl_pool {
            ctx.shutdown(ssl_pool).await;
        }
    }

    async fn call(
        &self,
        req: Connect,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Self::Res, Self::Error> {
        with_service(ctx.st().service(), async {
            match req.uri.scheme_str() {
                Some("https" | "wss") => {
                    if let Some(ref conn) = self.ssl_pool {
                        ctx.call(conn, req).await.into_error()
                    } else {
                        Err(Error::from(ClientError::from(
                            ConnectError::SslIsNotSupported,
                        )))
                    }
                }
                _ => ctx.call(&self.tcp_pool, req).await.into_error(),
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::client::{ClientConfig, ConnectorPipeline};
    use crate::service::{Pipeline, boxed, fn_service};
    use urly::Url;

    fn pool(calls: &Rc<Cell<usize>>, cfg: &SharedCfg) -> ConnectionPool {
        let calls = calls.clone();
        ConnectionPool::new(
            ConnectorPipeline::new(boxed::service(fn_service(move |_| {
                calls.set(calls.get() + 1);
                Box::pin(async { Err(Error::from(ConnectError::NoRecords)) })
            }))),
            cfg.get(),
        )
    }

    fn connect(uri: &'static str) -> Connect {
        Connect {
            uri: Url::from_static(uri),
            addr: None,
        }
    }

    #[crate::rt_test]
    async fn scheme_routing() {
        let cfg = SharedCfg::new("C").add(ClientConfig::new()).build();
        let (tcp, ssl) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));

        // without a secure connector tls requests are rejected
        let svc = Pipeline::new(
            cfg.clone(),
            Connector {
                tcp_pool: pool(&tcp, &cfg),
                ssl_pool: None,
            },
        );
        svc.ready().await.unwrap();
        for uri in ["https://localhost/", "wss://localhost/"] {
            let err = svc.call(connect(uri)).await.unwrap_err();
            assert!(matches!(
                err.into_error(),
                ClientError::Connect(ConnectError::SslIsNotSupported)
            ));
        }
        assert_eq!(tcp.get(), 0);
        let err = svc.call(connect("http://localhost/")).await.unwrap_err();
        assert!(matches!(
            err.into_error(),
            ClientError::Connect(ConnectError::NoRecords)
        ));
        assert_eq!(tcp.get(), 1);
        svc.shutdown().await;

        let svc = Pipeline::new(
            cfg.clone(),
            Connector {
                tcp_pool: pool(&tcp, &cfg),
                ssl_pool: Some(pool(&ssl, &cfg)),
            },
        );
        svc.ready().await.unwrap();
        let _ = svc.call(connect("wss://localhost/")).await.unwrap_err();
        let _ = svc.call(connect("ws://localhost/")).await.unwrap_err();
        assert_eq!((tcp.get(), ssl.get()), (2, 1));
        svc.shutdown().await;
    }
}
