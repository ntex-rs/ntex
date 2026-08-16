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
pub struct SslAcceptorFactory<St> {
    acceptor: ssl::SslAcceptor,
    st: PhantomData<St>,
}

impl<St> SslAcceptorFactory<St> {
    /// Create default openssl acceptor service
    pub fn new(acceptor: ssl::SslAcceptor) -> Self {
        SslAcceptorFactory {
            acceptor,
            st: PhantomData,
        }
    }
}

impl<St> fmt::Debug for SslAcceptorFactory<St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SslAcceptorFactory").finish()
    }
}

impl<St> From<ssl::SslAcceptor> for SslAcceptorFactory<St> {
    fn from(acceptor: ssl::SslAcceptor) -> Self {
        Self::new(acceptor)
    }
}

impl<F: Filter, St> ServiceFactory<Io<F>> for SslAcceptorFactory<St> {
    type St = St;
    type Res = Io<Layer<SslFilter, F>>;
    type Error = Box<dyn Error>;
    type Service = SslAcceptor<F, St>;
    type InitCfg = SharedCfg;
    type InitError = ();

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ok(SslAcceptor {
                acceptor: self.acceptor.clone(),
                conns: conns.clone(),
                cfg: cfg.get(),
                st: PhantomData,
            })
        })
    }
}

#[derive(Clone)]
/// Support `TLS` server connections via openssl package
///
/// `openssl` feature enables `Acceptor` type
pub struct SslAcceptor<F, St> {
    acceptor: ssl::SslAcceptor,
    cfg: Cfg<TlsConfig>,
    conns: Counter,
    st: PhantomData<(F, St)>,
}

impl<F, St> SslAcceptor<F, St> {
    /// Create default openssl acceptor service
    pub fn new(acceptor: ssl::SslAcceptor) -> Self {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| SslAcceptor {
            acceptor,
            conns: conns.clone(),
            cfg: Cfg::default(),
            st: PhantomData,
        })
    }
}

impl<F: Filter, St> Service for SslAcceptor<F, St> {
    type St = St;
    type Req = Io<F>;
    type Res = Io<Layer<SslFilter, F>>;
    type Error = Box<dyn Error>;

    async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        Ok(())
    }

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
        let _guard = self.conns.get();
        let ctx_result = ssl::Ssl::new(self.acceptor.context());

        let cfg: Cfg<TlsConfig> = io.shared().get();

        time::timeout(cfg.handshake_timeout(), async {
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

impl<F, St> From<ssl::SslAcceptor> for SslAcceptor<F, St> {
    fn from(acceptor: ssl::SslAcceptor) -> Self {
        Self::new(acceptor)
    }
}

impl<F, St> fmt::Debug for SslAcceptor<F, St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SslAcceptorService")
            .field("cfg", &self.cfg)
            .finish()
    }
}
