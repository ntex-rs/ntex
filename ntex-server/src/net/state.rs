#![allow(clippy::unused_async_trait_impl)]
use std::io;

pub trait ServerAppConfig: Sync + Send + 'static {
    type State: Clone;

    async fn create(&self) -> io::Result<Self::State>;
}

#[derive(Copy, Clone, Default, Debug)]
pub struct NoConfig;

impl ServerAppConfig for NoConfig {
    type State = ();

    async fn create(&self) -> io::Result<()> {
        Ok(())
    }
}

impl<F, Cfg> ServerAppConfig for F
where
    F: AsyncFn() -> io::Result<Cfg> + Sync + Send + 'static,
    Cfg: Clone,
{
    type State = Cfg;

    async fn create(&self) -> io::Result<Cfg> {
        (*self)().await
    }
}
