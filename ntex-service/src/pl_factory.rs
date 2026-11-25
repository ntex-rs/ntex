use std::{fmt, rc::Rc};

use crate::util::{CallFuture, CallPool, PoolOwner};
use crate::{ServiceFactory, pipeline::Pipeline};

/// Factory for a service pipeline.
///
/// Create futures are allocated in blocks that later creates reuse.
pub struct PipelineFactory<St, Req, Res, Err, InitErr> {
    f: Rc<dyn CreatePipeline<St, Req, Res, Err, InitErr>>,
}

trait CreatePipeline<St, Req, Res, Err, InitErr> {
    fn create(
        self: Rc<Self>,
        st: St,
    ) -> CallFuture<'static, Result<Pipeline<Req, Res, Err>, InitErr>>;
}

struct Factory<Sf> {
    sf: Sf,
    // Blocks of finished create futures. Declared last, so it is dropped after `sf`.
    pool: CallPool,
}

impl<Sf> PoolOwner for Rc<Factory<Sf>> {
    fn pool(&self) -> &CallPool {
        &self.pool
    }
}

impl<Sf, St, Req> CreatePipeline<St, Req, Sf::Res, Sf::Error, Sf::InitError> for Factory<Sf>
where
    Sf: ServiceFactory<St, Req> + 'static,
    St: 'static,
    Req: 'static,
    Sf::Res: 'static,
    Sf::Error: 'static,
{
    fn create(
        self: Rc<Self>,
        st: St,
    ) -> CallFuture<'static, Result<Pipeline<Req, Sf::Res, Sf::Error>, Sf::InitError>> {
        CallFuture::pooled(self, |factory| {
            let factory = factory.clone();
            async move {
                let svc = factory.sf.create(&st).await?;
                Ok(Pipeline::new(st, svc))
            }
        })
    }
}

impl<St, Req, Res, Err, InitErr> PipelineFactory<St, Req, Res, Err, InitErr> {
    /// Creates a pipeline factory from a service factory.
    pub fn new<Sf>(sf: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Res = Res, Error = Err, InitError = InitErr> + 'static,
        St: 'static,
        Req: 'static,
        Res: 'static,
        Err: 'static,
    {
        Self {
            f: Rc::new(Factory {
                sf,
                pool: CallPool::new(),
            }),
        }
    }

    /// Creates a service with `st` and wraps both in a [`Pipeline`].
    pub async fn create(&self, st: St) -> Result<Pipeline<Req, Res, Err>, InitErr> {
        self.f.clone().create(st).await
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
