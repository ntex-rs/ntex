use std::marker::PhantomData;

use ntex_util::future::BoxFuture;

pub(crate) trait StateFactory<St> {
    fn clo(&self) -> Box<dyn StateFactory<St> + Send>;

    fn create(&self) -> BoxFuture<'static, Result<St, &'static str>>;
}

pub(crate) fn state_factory<F, St>(f: F) -> Box<dyn StateFactory<St> + Send>
where
    F: AsyncFn() -> Result<St, &'static str> + Send + Clone + 'static,
    St: 'static,
{
    Box::new(StateFactoryImpl { f, st: PhantomData })
}

struct StateFactoryImpl<F, St> {
    f: F,
    st: PhantomData<St>,
}

unsafe impl<F, St> Send for StateFactoryImpl<F, St> where F: Send {}

impl<F, St> StateFactory<St> for StateFactoryImpl<F, St>
where
    F: AsyncFn() -> Result<St, &'static str> + Send + Clone + 'static,
    St: 'static,
{
    fn clo(&self) -> Box<dyn StateFactory<St> + Send> {
        state_factory(self.f.clone())
    }

    fn create(&self) -> BoxFuture<'static, Result<St, &'static str>> {
        let f = self.f.clone();
        Box::pin(async move { (f)().await })
    }
}
