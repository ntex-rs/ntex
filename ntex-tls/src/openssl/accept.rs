use std::{cell::RefCell, error::Error, fmt, io, marker::PhantomData};

use ntex_bytes::BytePages;
use ntex_io::{Filter, Io, Layer};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Ctx, ReadyCtx, Service, ServiceFactory};
use ntex_util::{services::Counter, time};
use tls_openssl::ssl;

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, openssl::SslFilter};

#[derive(Clone)]
/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct SslAcceptor<F> {
    acceptor: ssl::SslAcceptor,
    _t: PhantomData<F>,
}

impl<F> SslAcceptor<F> {
    /// Create default openssl acceptor service
    pub fn new(acceptor: ssl::SslAcceptor) -> Self {
        SslAcceptor {
            acceptor,
            _t: PhantomData,
        }
    }
}

impl<F> fmt::Debug for SslAcceptor<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SslAcceptor").finish()
    }
}

impl<F> From<ssl::SslAcceptor> for SslAcceptor<F> {
    fn from(acceptor: ssl::SslAcceptor) -> Self {
        Self::new(acceptor)
    }
}

impl<F: Filter, St> ServiceFactory<St> for SslAcceptor<F> {
    type Req = Io<F>;
    type Res = Io<Layer<SslFilter, F>>;
    type Error = Box<dyn Error>;
    type Service = SslAcceptorService<F>;
    type InitCfg = SharedCfg;
    type InitError = ();

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ok(SslAcceptorService {
                acceptor: self.acceptor.clone(),
                conns: conns.clone(),
                cfg: cfg.get(),
                _t: PhantomData,
            })
        })
    }
}

#[derive(Clone)]
/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct SslAcceptorService<F> {
    acceptor: ssl::SslAcceptor,
    cfg: Cfg<TlsConfig>,
    conns: Counter,
    _t: PhantomData<F>,
}

impl<St, F: Filter> Service<St> for SslAcceptorService<F> {
    type Req = Io<F>;
    type Res = Io<Layer<SslFilter, F>>;
    type Error = Box<dyn Error>;

    async fn ready(&self, _: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        Ok(())
    }

    async fn call(
        &self,
        io: Io<F>,
        _: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        let _guard = self.conns.get();
        let ctx_result = ssl::Ssl::new(self.acceptor.context());

        time::timeout(self.cfg.handshake_timeout(), async {
            let ssl = ctx_result.map_err(super::map_to_ioerr)?;
            let inner = super::IoInner {
                source: None,
                destination: BytePages::new(io.cfg().write_page_size()),
            };
            let mut stream = ssl::SslStream::new(ssl, inner)?;
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
        .map_err(|()| {
            io::Error::new(io::ErrorKind::TimedOut, "ssl handshake timeout").into()
        })
        .and_then(|item| item)
    }
}

impl<F> fmt::Debug for SslAcceptorService<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SslAcceptorService")
            .field("cfg", &self.cfg)
            .finish()
    }
}
