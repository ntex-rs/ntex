use std::{cell::RefCell, fmt, io, marker::PhantomData};

use ntex_bytes::BytePages;
use ntex_io::{Filter, Io, Layer};
use ntex_service::cfg::Cfg;
use ntex_service::{Ctx, ReadyCtx, Service, cfg::Configuration};
use ntex_util::{services::Counter, time};
use tls_openssl::ssl;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, openssl::SslFilter};

#[derive(Clone)]
/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct SslAcceptor<F> {
    acceptor: ssl::SslAcceptor,
    conns: Counter,
    st: PhantomData<F>,
}

impl<F> SslAcceptor<F> {
    /// Create default openssl acceptor service
    pub fn new(acceptor: ssl::SslAcceptor) -> Self {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| SslAcceptor {
            acceptor,
            conns: conns.clone(),
            st: PhantomData,
        })
    }
}

impl<F: Filter, St> Service<St> for SslAcceptor<F> {
    type Req = Io<F>;
    type Res = Io<Layer<SslFilter, F>>;
    type Error = io::Error;

    async fn ready(&self, _: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        Ok(())
    }

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let _guard = self.conns.get();
        let ssl = ssl::Ssl::new(self.acceptor.context()).map_err(io::Error::other)?;
        let cfg: Cfg<TlsConfig> = io.cfg().ctx().get();

        time::timeout(cfg.handshake_timeout(), async {
            let inner = super::IoInner {
                source: None,
                destination: BytePages::new(io.cfg().write_page_size()),
            };
            let mut stream = ssl::SslStream::new(ssl, inner).map_err(io::Error::other)?;
            let _ = stream.accept();

            let filter = SslFilter {
                inner: RefCell::new(stream),
            };
            let io = io.add_filter(filter);

            log::trace!("Accepting tls connection");
            loop {
                let result = io.with_buf(|buf| {
                    let filter = io.filter();
                    filter.with_buffers(buf, |_| filter.inner.borrow_mut().accept())
                })?;
                if super::handle_result(&io, result).await?.is_some() {
                    break;
                }
            }

            Ok(io)
        })
        .await
        .map_err(|()| io::Error::new(io::ErrorKind::TimedOut, "ssl handshake timeout"))
        .and_then(|item| item)
    }
}

impl<F> From<ssl::SslAcceptor> for SslAcceptor<F> {
    fn from(acceptor: ssl::SslAcceptor) -> Self {
        Self::new(acceptor)
    }
}

impl<F> fmt::Debug for SslAcceptor<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SslAcceptor").finish()
    }
}
