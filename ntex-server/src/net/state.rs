use std::io;

pub trait ServerAppConfig: Sync + Send + 'static {
    type Config: Clone;

    async fn create(&self) -> io::Result<Self::Config>;
}

#[derive(Copy, Clone, Default, Debug)]
pub struct NoConfig;

impl ServerAppConfig for NoConfig {
    type Config = ();

    async fn create(&self) -> io::Result<()> {
        Ok(())
    }
}

impl<F, Cfg> ServerAppConfig for F
where
    F: AsyncFn() -> io::Result<Cfg> + Sync + Send + 'static,
    Cfg: Clone,
{
    type Config = Cfg;

    async fn create(&self) -> io::Result<Cfg> {
        (*self)().await
    }
}
