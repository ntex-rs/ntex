use std::rc::Rc;

use crate::{SharedCfg, http::HeaderMap, time::Millis};

#[derive(Clone, Debug)]
pub struct ClientConfig(Rc<ClientConfigInner>);

#[derive(Debug)]
pub(super) struct ClientConfigInner {
    pub(super) cfg: SharedCfg,
    pub(super) headers: HeaderMap,
    pub(super) timeout: Millis,
    pub(super) pl_limit: usize,
    pub(super) pl_timeout: Millis,
    pub(super) default_headers: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self(Rc::new(ClientConfigInner::default()))
    }
}

impl Default for ClientConfigInner {
    fn default() -> Self {
        Self {
            cfg: SharedCfg::default(),
            headers: HeaderMap::new(),
            timeout: Millis(5_000),
            pl_limit: 262_144,
            pl_timeout: Millis(10_000),
            default_headers: true,
        }
    }
}

impl ClientConfig {
    pub(super) fn new(cfg: ClientConfigInner) -> Self {
        Self(Rc::new(cfg))
    }

    pub fn cfg(&self) -> SharedCfg {
        self.0.cfg
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.0.headers
    }

    pub fn timeout(&self) -> Millis {
        self.0.timeout
    }

    pub fn payload_limit(&self) -> usize {
        self.0.pl_limit
    }

    pub fn payload_timeout(&self) -> Millis {
        self.0.pl_timeout
    }
}
