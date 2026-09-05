use std::{fmt, rc::Rc};

use crate::{ServiceFactory, pipeline::Pipeline, util::BoxFuture};

/// Factory for a service pipeline.
pub struct PipelineFactory<St, Req, Res, Err, InitErr> {
    f: Rc<dyn Fn(St) -> BoxFuture<'static, Result<Pipeline<Req, Res, Err>, InitErr>>>,
}

impl<St, Req, Res, Err, InitErr> PipelineFactory<St, Req, Res, Err, InitErr> {
    pub fn new<Sf>(sf: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Res = Res, Error = Err, InitError = InitErr> + 'static,
        St: 'static,
        Req: 'static,
        Res: 'static,
        Err: 'static,
    {
        let sf = Rc::new(sf);
        Self {
            f: Rc::new(move |st: St| {
                let sf = sf.clone();
                Box::pin(async move {
                    let svc = sf.create(&st).await?;
                    Ok(Pipeline::new(st, svc))
                })
            }),
        }
    }

    pub async fn create(&self, st: St) -> Result<Pipeline<Req, Res, Err>, InitErr> {
        (self.f)(st).await
    }
}

impl<St, Req, Res, Err, InitErr> Clone for PipelineFactory<St, Req, Res, Err, InitErr> {
    fn clone(&self) -> Self {
        PipelineFactory { f: self.f.clone() }
    }
}

impl<St, Req, Res, Err, InitErr> fmt::Debug for PipelineFactory<St, Req, Res, Err, InitErr> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineFactory").finish()
    }
}
