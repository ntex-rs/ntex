use std::{future::Future, io, marker::PhantomData, sync::Arc};

use tls_rustls::ServerConfig;

use ntex_io::{Filter, Io, Layer};
use ntex_service::cfg::{Cfg, SharedCfg};
use ntex_service::{Service, ServiceCtx, ServiceFactory};
use ntex_util::{future::Ready, services::Counter};

use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig, rustls::TlsServerFilter};

#[derive(Clone, Debug)]
/// Support `TLS` connections via rustls package
///
/// `rust-tls` feature enables `TlsAcceptor` type
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
}

impl TlsAcceptor {
    /// Create rustls based `Acceptor` service factory
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self { config }
    }
}

impl From<ServerConfig> for TlsAcceptor {
    fn from(cfg: ServerConfig) -> Self {
        Self::new(Arc::new(cfg))
    }
}

impl<F: Filter, St> ServiceFactory<St, Io<F>> for TlsAcceptor {
    type Res = Io<Layer<TlsServerFilter, F>>;
    type Error = io::Error;
    type Service = TlsAcceptorService<F, St>;
    type InitCfg = SharedCfg;
    type InitError = ();

    fn create(
        &self,
        cfg: &SharedCfg,
    ) -> impl Future<Output = Result<Self::Service, Self::InitError>> {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| {
            Ready::Ok(TlsAcceptorService {
                cfg: cfg.get(),
                config: self.config.clone(),
                conns: conns.clone(),
                _t: PhantomData,
            })
        })
    }
}

#[derive(Debug)]
/// `RusTLS` based `Acceptor` service
pub struct TlsAcceptorService<F, St> {
    cfg: Cfg<TlsConfig>,
    config: Arc<ServerConfig>,
    conns: Counter,
    _t: PhantomData<(F, St)>,
}

impl<F: Filter, St> Service for TlsAcceptorService<F, St> {
    type St = St;
    type Req = Io<F>;
    type Res = Io<Layer<TlsServerFilter, F>>;
    type Error = io::Error;

    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        Ok(())
    }

    async fn call(
        &self,
        io: Io<F>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Res, Self::Error> {
        let _guard = self.conns.get();
        super::TlsServerFilter::create(
            io,
            self.config.clone(),
            self.cfg.handshake_timeout(),
        )
        .await
    }
}
