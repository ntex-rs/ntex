use std::{cell::RefCell, error::Error, fmt, io};

use ntex_io::{Cfg, Filter, Io, Layer, SharedConfig};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::{services::Counter, time};
use tls_openssl::ssl;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, openssl::SslFilter};

#[derive(Clone)]
/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct SslAcceptor {
    acceptor: ssl::SslAcceptor,
}

impl SslAcceptor {
    /// Create default openssl acceptor service
    pub fn new(acceptor: ssl::SslAcceptor) -> Self {
        SslAcceptor { acceptor }
    }
}

impl fmt::Debug for SslAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SslAcceptor").finish()
    }
}

impl From<ssl::SslAcceptor> for SslAcceptor {
    fn from(acceptor: ssl::SslAcceptor) -> Self {
        Self::new(acceptor)
    }
}

impl<F: Filter> ServiceFactory<Io<F>, SharedConfig> for SslAcceptor {
    type Response = Io<Layer<SslFilter, F>>;
    type Error = Box<dyn Error>;
    type Service = SslAcceptorService;
    type InitError = ();

    async fn create(&self, cfg: SharedConfig) -> Result<Self::Service, Self::InitError> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ok(SslAcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.clone(),
                cfg: cfg.get(),
            })
        })
    }
}

#[derive(Clone)]
/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct SslAcceptorService {
    acceptor: ssl::SslAcceptor,
    cfg: Cfg<TlsConfig>,
    conns: Counter,
}

impl<F: Filter> Service<Io<F>> for SslAcceptorService {
    type Response = Io<Layer<SslFilter, F>>;
    type Error = Box<dyn Error>;

    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await
        }
        Ok(())
    }

    async fn call(
        &self,
        io: Io<F>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let _guard = self.conns.get();
        let ctx_result = ssl::Ssl::new(self.acceptor.context());

        time::timeout(self.cfg.handshake_timeout(), async {
            let ssl = ctx_result.map_err(super::map_to_ioerr)?;
            let inner = super::IoInner {
                source: None,
                destination: None,
            };
            let filter = SslFilter {
                inner: RefCell::new(ssl::SslStream::new(ssl, inner)?),
            };
            let io = io.add_filter(filter);

            log::trace!("Accepting tls connection");
            loop {
                let result = io.with_buf(|buf| {
                    let filter = io.filter();
                    filter.with_buffers(buf, || filter.inner.borrow_mut().accept())
                })?;
                if super::handle_result(&io, result).await?.is_some() {
                    break;
                }
            }

            Ok(io)
        })
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "ssl handshake timeout").into()
        })
        .and_then(|item| item)
    }
}

impl fmt::Debug for SslAcceptorService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SslAcceptorService")
            .field("cfg", &self.cfg)
            .finish()
    }
}
