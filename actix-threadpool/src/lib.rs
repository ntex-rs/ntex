//! Thread pool for blocking operations

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use derive_more::Display;
use futures::channel::oneshot;
use parking_lot::Mutex;
use threadpool::ThreadPool;

/// Env variable for default cpu pool size.
const ENV_CPU_POOL_VAR: &str = "ACTIX_THREADPOOL";

lazy_static::lazy_static! {
    pub(crate) static ref DEFAULT_POOL: Mutex<ThreadPool> = {
        let num = std::env::var(ENV_CPU_POOL_VAR)
            .map_err(|_| ())
            .and_then(|val| {
                val.parse().map_err(|_| log::warn!(
                    "Can not parse {} value, using default",
                    ENV_CPU_POOL_VAR,
                ))
            })
            .unwrap_or_else(|_| num_cpus::get() * 5);
        Mutex::new(
            threadpool::Builder::new()
                .thread_name("actix-web".to_owned())
                .num_threads(num)
                .build(),
        )
    };
}

thread_local! {
    static POOL: ThreadPool = {
        DEFAULT_POOL.lock().clone()
    };
}

/// Error of blocking operation execution being cancelled.
#[derive(Clone, Copy, Debug, Display)]
#[display(fmt = "Thread pool is gone")]
pub struct Cancelled;

/// Execute blocking function on a thread pool, returns future that resolves
/// to result of the function execution.
pub fn run<F, I>(f: F) -> CpuFuture<I>
where
    F: FnOnce() -> I + Send + 'static,
    I: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    POOL.with(|pool| {
        pool.execute(move || {
            if !tx.is_canceled() {
                let _ = tx.send(f());
            }
        })
    });

    CpuFuture { rx }
}

/// Blocking operation completion future. It resolves with results
/// of blocking function execution.
pub struct CpuFuture<I> {
    rx: oneshot::Receiver<I>,
}

impl<I> Future for CpuFuture<I> {
    type Output = Result<I, Cancelled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let rx = Pin::new(&mut Pin::get_mut(self).rx);
        let res = futures::ready!(rx.poll(cx));
        Poll::Ready(res.map_err(|_| Cancelled))
    }
}
