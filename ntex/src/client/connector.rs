use crate::error::{Error, ErrorMapping, with_service};
use crate::{Ctx, Service, SharedCfg, util::join};

use super::error::{ClientError, ConnectError};
use super::{Connect, Connection, pool::ConnectionPool};

#[derive(Debug)]
/// Manages http client network connectivity.
///
/// The `Connector` type uses a builder-like combinator pattern for service
/// construction that finishes by calling the `.finish()` method.
///
/// ```rust,no_run
/// use ntex::client::Connector;
///
/// let connector = Connector::default()
///      .keep_alive(5_000);
/// ```
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
