//! A runtime implementation that runs everything on the current thread.
use std::{cell::RefCell, ptr};

mod arbiter;
mod builder;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::system::System;

thread_local! {
    static CB: RefCell<(TBefore, TSpawn, TAfter)> = RefCell::new((
        Box::new(|| {None}), Box::new(|_| {ptr::null()}), Box::new(|_| {}))
    );
}

type TBefore = Box<dyn Fn() -> Option<*const ()>>;
type TSpawn = Box<dyn Fn(*const ()) -> *const ()>;
type TAfter = Box<dyn Fn(*const ())>;

pub unsafe fn spawn_cbs<FBefore, FSpawn, FAfter>(
    before: FBefore,
    before_spawn: FSpawn,
    after_spawn: FAfter,
) where
    FBefore: Fn() -> Option<*const ()> + 'static,
    FSpawn: Fn(*const ()) -> *const () + 'static,
    FAfter: Fn(*const ()) + 'static,
{
    CB.with(|cb| {
        *cb.borrow_mut() = (
            Box::new(before),
            Box::new(before_spawn),
            Box::new(after_spawn),
        );
    });
}

#[allow(dead_code)]
#[cfg(all(feature = "glommio", target_os = "linux"))]
mod glommio {
    use std::{future::Future, pin::Pin, task::Context, task::Poll};

    use futures_channel::oneshot::Canceled;
    use glomm_io::task;

    pub type JoinError = Canceled;

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
        let ptr = crate::CB.with(|cb| (cb.borrow().0)());
        JoinHandle {
            fut: Either::Left(
                glomm_io::spawn_local(async move {
                    if let Some(ptr) = ptr {
                        let new_ptr = crate::CB.with(|cb| (cb.borrow().1)(ptr));
                        glomm_io::executor().yield_now().await;
                        let res = f.await;
                        crate::CB.with(|cb| (cb.borrow().2)(new_ptr));
                        res
                    } else {
                        glomm_io::executor().yield_now().await;
                        f.await
                    }
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

    pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let fut = glomm_io::executor().spawn_blocking(f);
        JoinHandle {
            fut: Either::Right(Box::pin(async move { Ok(fut.await) })),
        }
    }

    enum Either<T1, T2> {
        Left(T1),
        Right(T2),
    }

    /// Blocking operation completion future. It resolves with results
    /// of blocking function execution.
    pub struct JoinHandle<T> {
        fut:
            Either<task::JoinHandle<T>, Pin<Box<dyn Future<Output = Result<T, Canceled>>>>>,
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
            // .unhandled_panic(tok_io::runtime::UnhandledPanic::ShutdownRuntime)
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
        let ptr = crate::CB.with(|cb| (cb.borrow().0)());
        tok_io::task::spawn_local(async move {
            if let Some(ptr) = ptr {
                let new_ptr = crate::CB.with(|cb| (cb.borrow().1)(ptr));
                let res = f.await;
                crate::CB.with(|cb| (cb.borrow().2)(new_ptr));
                res
            } else {
                f.await
            }
        })
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
    use std::{fmt, future::Future, pin::Pin, task::Context, task::Poll};

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
        let ptr = crate::CB.with(|cb| (cb.borrow().0)());
        JoinHandle {
            fut: async_std::task::spawn_local(async move {
                if let Some(ptr) = ptr {
                    let new_ptr = crate::CB.with(|cb| (cb.borrow().1)(ptr));
                    let res = f.await;
                    crate::CB.with(|cb| (cb.borrow().2)(new_ptr));
                    res
                } else {
                    f.await
                }
            }),
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

    #[derive(Debug, Copy, Clone)]
    pub struct JoinError;

    impl fmt::Display for JoinError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "JoinError")
        }
    }

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

#[cfg(all(
    not(feature = "tokio"),
    not(feature = "async-std"),
    not(feature = "glommio")
))]
mod spawn_blocking_stub {
    use std::fmt;

    #[derive(Debug, Copy, Clone)]
    pub struct JoinError;

    impl fmt::Display for JoinError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "JoinError")
        }
    }

    impl std::error::Error for JoinError {}

    pub fn spawn_blocking<F, T>(
        _: F,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, JoinError>>>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        unimplemented!()
    }
}
#[cfg(all(
    not(feature = "tokio"),
    not(feature = "async-std"),
    not(feature = "glommio")
))]
pub use self::spawn_blocking_stub::*;
