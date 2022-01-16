//! A runtime implementation that runs everything on the current thread.
#![allow(clippy::return_self_not_must_use)]

mod arbiter;
mod builder;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::system::System;

#[allow(dead_code)]
#[cfg(all(feature = "glommio", target_os = "linux"))]
mod glommio {
    use std::{future::Future, pin::Pin, task::Context, task::Poll};

    use futures_channel::oneshot::{self, Canceled};
    use glomm_io::{task, Task};
    use once_cell::sync::Lazy;
    use parking_lot::Mutex;
    use threadpool::ThreadPool;

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        let ex = glomm_io::LocalExecutor::default();
        ex.run(async move {
            let _ = fut.await;
        })
    }

    /// Spawn a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for spawning futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        JoinHandle {
            fut: Either::Left(
                Task::local(async move {
                    let _ = Task::<()>::later().await;
                    f.await
                })
                .detach(),
            ),
        }
    }

    /// Executes a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for executing futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn_fn<F, R>(f: F) -> JoinHandle<R::Output>
    where
        F: FnOnce() -> R + 'static,
        R: Future + 'static,
    {
        spawn(async move { f().await })
    }

    /// Env variable for default cpu pool size.
    const ENV_CPU_POOL_VAR: &str = "THREADPOOL";

    static DEFAULT_POOL: Lazy<Mutex<ThreadPool>> = Lazy::new(|| {
        let num = std::env::var(ENV_CPU_POOL_VAR)
            .map_err(|_| ())
            .and_then(|val| {
                val.parse().map_err(|_| {
                    log::warn!("Can not parse {} value, using default", ENV_CPU_POOL_VAR,)
                })
            })
            .unwrap_or_else(|_| num_cpus::get() * 5);
        Mutex::new(
            threadpool::Builder::new()
                .thread_name("ntex".to_owned())
                .num_threads(num)
                .build(),
        )
    });

    thread_local! {
        static POOL: ThreadPool = {
            DEFAULT_POOL.lock().clone()
        };
    }

    enum Either<T1, T2> {
        Left(T1),
        Right(T2),
    }

    /// Blocking operation completion future. It resolves with results
    /// of blocking function execution.
    pub struct JoinHandle<T> {
        fut: Either<task::JoinHandle<T>, oneshot::Receiver<T>>,
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, Canceled>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.fut {
                Either::Left(ref mut f) => match Pin::new(f).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(res) => Poll::Ready(res.ok_or(Canceled)),
                },
                Either::Right(ref mut f) => Pin::new(f).poll(cx),
            }
        }
    }

    pub fn spawn_blocking<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        POOL.with(|pool| {
            pool.execute(move || {
                if !tx.is_canceled() {
                    let _ = tx.send(f());
                }
            })
        });

        JoinHandle {
            fut: Either::Right(rx),
        }
    }
}

#[cfg(feature = "tokio")]
mod tokio {
    use std::future::Future;
    pub use tok_io::task::{spawn_blocking, JoinError, JoinHandle};

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        let rt = tok_io::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tok_io::task::LocalSet::new().block_on(&rt, fut);
    }

    /// Spawn a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for spawning futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn<F>(f: F) -> tok_io::task::JoinHandle<F::Output>
    where
        F: Future + 'static,
    {
        tok_io::task::spawn_local(f)
    }

    /// Executes a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for executing futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn_fn<F, R>(f: F) -> tok_io::task::JoinHandle<R::Output>
    where
        F: FnOnce() -> R + 'static,
        R: Future + 'static,
    {
        spawn(async move { f().await })
    }
}

#[allow(dead_code)]
#[cfg(feature = "async-std")]
mod asyncstd {
    use futures_core::ready;
    use std::{future::Future, pin::Pin, task::Context, task::Poll};

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        async_std::task::block_on(fut);
    }

    /// Spawn a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for spawning futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
    {
        JoinHandle {
            fut: async_std::task::spawn_local(f),
        }
    }

    /// Executes a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for executing futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    pub fn spawn_fn<F, R>(f: F) -> JoinHandle<R::Output>
    where
        F: FnOnce() -> R + 'static,
        R: Future + 'static,
    {
        spawn(async move { f().await })
    }

    /// Spawns a blocking task.
    ///
    /// The task will be spawned onto a thread pool specifically dedicated
    /// to blocking tasks. This is useful to prevent long-running synchronous
    /// operations from blocking the main futures executor.
    pub fn spawn_blocking<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        JoinHandle {
            fut: async_std::task::spawn_blocking(f),
        }
    }

    #[derive(Debug, Copy, Clone, derive_more::Display)]
    pub struct JoinError;
    impl std::error::Error for JoinError {}

    pub struct JoinHandle<T> {
        fut: async_std::task::JoinHandle<T>,
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(Ok(ready!(Pin::new(&mut self.fut).poll(cx))))
        }
    }
}

#[cfg(feature = "tokio")]
pub use self::tokio::*;

#[cfg(all(
    not(feature = "tokio"),
    not(feature = "glommio"),
    feature = "async-std",
    target_os = "linux"
))]
pub use self::asyncstd::*;

#[cfg(all(
    not(feature = "tokio"),
    not(feature = "async-std"),
    feature = "glommio"
))]
pub use self::glommio::*;

/// Runs the provided future, blocking the current thread until the future
/// completes.
#[cfg(all(
    not(feature = "tokio"),
    not(feature = "async-std"),
    not(feature = "glommio")
))]
pub fn block_on<F: std::future::Future<Output = ()>>(_: F) {
    panic!("async runtime is not configured");
}

#[cfg(all(
    not(feature = "tokio"),
    not(feature = "async-std"),
    not(feature = "glommio")
))]
pub fn spawn<F>(_: F) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output>>>
where
    F: std::future::Future + 'static,
{
    unimplemented!()
}
