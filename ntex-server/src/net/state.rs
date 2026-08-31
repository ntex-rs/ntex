use std::{io, sync::Arc};

use ntex_util::future::BoxFuture;

pub(crate) type StateFactory<St> =
    Arc<dyn Fn() -> BoxFuture<'static, io::Result<St>> + Send + Sync>;

pub(crate) fn state_factory<F, St>(f: F) -> StateFactory<St>
where
    F: ServerStateFactory<St>,
{
    let st = Arc::new(f);

    Arc::new(move || {
        let st = st.clone();
        Box::pin(async move { st.create().await })
    })
}

pub trait ServerStateFactory<St>: Sync + Send + 'static {
    async fn create(&self) -> io::Result<St>;
}

impl<F, St> ServerStateFactory<St> for F
where
    F: AsyncFn() -> io::Result<St> + Sync + Send + 'static,
{
    async fn create(&self) -> io::Result<St> {
        (*self)().await
    }
}
