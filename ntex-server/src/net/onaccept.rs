use std::{fmt, marker::PhantomData, sync::Arc};

use ntex_util::future::BoxFuture;

use super::socket::Stream;

pub(crate) trait OnWorkerStart {
    fn clone_fn(&self) -> Box<dyn OnWorkerStart + Send>;

    fn run(&self) -> BoxFuture<'static, Result<(), &'static str>>;
}

pub(super) struct OnWorkerStartWrapper<F> {
    pub(super) f: F,
}

unsafe impl<F> Send for OnWorkerStartWrapper<F> where F: Send {}

impl<F> OnWorkerStartWrapper<F>
where
    F: AsyncFn() -> Result<(), &'static str> + Send + Clone + 'static,
{
    pub(super) fn create(f: F) -> Box<dyn OnWorkerStart + Send> {
        Box::new(Self { f })
    }
}

impl<F> OnWorkerStart for OnWorkerStartWrapper<F>
where
    F: AsyncFn() -> Result<(), &'static str> + Send + Clone + 'static,
{
    fn clone_fn(&self) -> Box<dyn OnWorkerStart + Send> {
        Box::new(Self { f: self.f.clone() })
    }

    fn run(&self) -> BoxFuture<'static, Result<(), &'static str>> {
        let f = self.f.clone();
        Box::pin(async move { (f)().await })
    }
}

pub(crate) trait OnAccept {
    fn clone_fn(&self) -> Box<dyn OnAccept + Send>;

    fn run(&self, name: Arc<str>, stream: Stream) -> BoxFuture<'static, Result<Stream, ()>>;
}

pub(super) struct OnAcceptWrapper<F, E> {
    pub(super) f: F,
    pub(super) _t: PhantomData<E>,
}

unsafe impl<F, E> Send for OnAcceptWrapper<F, E> where F: Send {}

impl<F, E> OnAcceptWrapper<F, E>
where
    F: AsyncFn(Arc<str>, Stream) -> Result<Stream, E> + Send + Clone + 'static,
    E: fmt::Display + 'static,
{
    pub(super) fn create(f: F) -> Box<dyn OnAccept + Send> {
        Box::new(Self { f, _t: PhantomData })
    }
}

impl<F, E> OnAccept for OnAcceptWrapper<F, E>
where
    F: AsyncFn(Arc<str>, Stream) -> Result<Stream, E> + Send + Clone + 'static,
    E: fmt::Display + 'static,
{
    fn clone_fn(&self) -> Box<dyn OnAccept + Send> {
        Box::new(Self {
            f: self.f.clone(),
            _t: PhantomData,
        })
    }

    fn run(&self, name: Arc<str>, stream: Stream) -> BoxFuture<'static, Result<Stream, ()>> {
        let f = self.f.clone();
        Box::pin(async move {
            (f)(name, stream).await.map_err(|e| {
                log::error!("On accept callback failed: {e}");
            })
        })
    }
}
