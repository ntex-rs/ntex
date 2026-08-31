use std::{fmt, rc::Rc};

use crate::pipeline::{Pipeline, PipelineState};
use crate::{ServiceFactory, util::BoxFuture};

/// Factory for a service pipeline.
pub struct PipelineFactory<St, Req, Res, Err, InitCfg, InitErr> {
    f: Rc<
        dyn for<'r> Fn(&'r InitCfg, St) -> BoxFuture<'r, Result<Pipeline<Req, Res, Err>, InitErr>>,
    >,
}

impl<St, Req, Res, Err, InitCfg, InitErr> PipelineFactory<St, Req, Res, Err, InitCfg, InitErr> {
    pub fn new<Sf>(sf: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, InitCfg, Res = Res, Error = Err, InitError = InitErr> + 'static,
        St: 'static,
        Req: 'static,
        Res: 'static,
        Err: 'static,
        InitCfg: 'static,
    {
        let sf = Rc::new(sf);
        Self {
            f: Rc::new(move |cfg: &InitCfg, st: St| {
                let sf = sf.clone();
                Box::pin(async move { Ok(Pipeline::with(st, sf.create(cfg).await?)) })
            }),
        }
    }

    pub async fn create(&self, cfg: &InitCfg, st: St) -> Result<Pipeline<Req, Res, Err>, InitErr> {
        (self.f)(cfg, st).await
    }
}

impl<St, Req, Res, Err, InitCfg, InitErr> Clone
    for PipelineFactory<St, Req, Res, Err, InitCfg, InitErr>
{
    fn clone(&self) -> Self {
        PipelineFactory { f: self.f.clone() }
    }
}

impl<St, Req, Res, Err, Cfg, InitErr> fmt::Debug
    for PipelineFactory<St, Req, Res, Err, Cfg, InitErr>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineFactory").finish()
    }
}

/// Factory for a service pipeline with state.
pub struct PipelineStateFactory<St, Req, Res, Err, InitCfg, InitErr> {
    f: Rc<
        dyn for<'r> Fn(
            &'r InitCfg,
        ) -> BoxFuture<'r, Result<PipelineState<St, Req, Res, Err>, InitErr>>,
    >,
}

impl<St, Req, Res, Err, InitCfg, InitErr> PipelineStateFactory<St, Req, Res, Err, InitCfg, InitErr>
where
    St: 'static,
{
    pub fn new<Sf>(sf: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, InitCfg, Res = Res, Error = Err, InitError = InitErr> + 'static,
        Req: 'static,
        Res: 'static,
        Err: 'static,
        InitCfg: 'static,
    {
        let sf = Rc::new(sf);
        Self {
            f: Rc::new(move |cfg: &InitCfg| {
                let sf = sf.clone();
                Box::pin(async move { Ok(PipelineState::new(sf.create(cfg).await?)) })
            }),
        }
    }

    pub async fn create(&self, cfg: &InitCfg) -> Result<PipelineState<St, Req, Res, Err>, InitErr> {
        (self.f)(cfg).await
    }
}

impl<St, Req, Res, Err, InitCfg, InitErr> Clone
    for PipelineStateFactory<St, Req, Res, Err, InitCfg, InitErr>
{
    fn clone(&self) -> Self {
        PipelineStateFactory { f: self.f.clone() }
    }
}

impl<St, Req, Res, Err, Cfg, InitErr> fmt::Debug
    for PipelineStateFactory<St, Req, Res, Err, Cfg, InitErr>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineStateFactory").finish()
    }
}
