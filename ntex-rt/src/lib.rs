#![allow(clippy::type_complexity, clippy::let_underscore_future)]
//! A runtime implementation that runs everything on the current thread.
use std::{cell::RefCell, ptr};

mod arbiter;
mod builder;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::system::System;

thread_local! {
    static CB: RefCell<(TBefore, TEnter, TExit, TAfter)> = RefCell::new((
        Box::new(|| {None}), Box::new(|_| {ptr::null()}), Box::new(|_| {}), Box::new(|_| {}))
    );
}

type TBefore = Box<dyn Fn() -> Option<*const ()>>;
type TEnter = Box<dyn Fn(*const ()) -> *const ()>;
type TExit = Box<dyn Fn(*const ())>;
type TAfter = Box<dyn Fn(*const ())>;

/// # Safety
///
/// The user must ensure that the pointer returned by `before` is `'static`. It will become
/// owned by the spawned task for the life of the task. Ownership of the pointer will be
/// returned to the user at the end of the task via `after`. The pointer is opaque to the
/// runtime.
pub unsafe fn spawn_cbs<FBefore, FEnter, FExit, FAfter>(
    before: FBefore,
    enter: FEnter,
    exit: FExit,
    after: FAfter,
) where
    FBefore: Fn() -> Option<*const ()> + 'static,
    FEnter: Fn(*const ()) -> *const () + 'static,
    FExit: Fn(*const ()) + 'static,
    FAfter: Fn(*const ()) + 'static,
{
    CB.with(|cb| {
        *cb.borrow_mut() = (
            Box::new(before),
            Box::new(enter),
            Box::new(exit),
            Box::new(after),
        );
    });
}

#[allow(dead_code)]
#[cfg(feature = "tokio")]
mod tokio {
    use std::future::{poll_fn, Future};
    use tok_io::runtime::Handle;
    pub use tok_io::task::{spawn_blocking, JoinError, JoinHandle};

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        if let Ok(hnd) = Handle::try_current() {
            log::debug!("Use existing tokio runtime and block on future");
            hnd.block_on(tok_io::task::LocalSet::new().run_until(fut));
        } else {
            log::debug!("Create tokio runtime and block on future");

            let rt = tok_io::runtime::Builder::new_current_thread()
                .enable_all()
                //.unhandled_panic(tok_io::runtime::UnhandledPanic::ShutdownRuntime)
                .build()
                .unwrap();
            tok_io::task::LocalSet::new().block_on(&rt, fut);
        }
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
                tok_io::pin!(f);
                let result = poll_fn(|ctx| {
                    let new_ptr = crate::CB.with(|cb| (cb.borrow().1)(ptr));
                    let result = f.as_mut().poll(ctx);
                    crate::CB.with(|cb| (cb.borrow().2)(new_ptr));
                    result
                })
                .await;
                crate::CB.with(|cb| (cb.borrow().3)(ptr));
                result
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
#[cfg(feature = "compio")]
mod compio {
    use std::task::{ready, Context, Poll};
    use std::{fmt, future::poll_fn, future::Future, pin::Pin};

    use compio_runtime::Runtime;

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        log::info!(
            "Starting compio runtime, driver {:?}",
            compio_driver::DriverType::current()
        );
        let rt = Runtime::new().unwrap();
        rt.block_on(fut);
    }

    /// Spawns a blocking task.
    ///
    /// The task will be spawned onto a thread pool specifically dedicated
    /// to blocking tasks. This is useful to prevent long-running synchronous
    /// operations from blocking the main futures executor.
    pub fn spawn_blocking<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + Sync + 'static,
        T: Send + 'static,
    {
        JoinHandle {
            fut: Some(compio_runtime::spawn_blocking(f)),
        }
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
        let fut = compio_runtime::spawn(async move {
            if let Some(ptr) = ptr {
                let mut f = std::pin::pin!(f);
                let result = poll_fn(|ctx| {
                    let new_ptr = crate::CB.with(|cb| (cb.borrow().1)(ptr));
                    let result = f.as_mut().poll(ctx);
                    crate::CB.with(|cb| (cb.borrow().2)(new_ptr));
                    result
                })
                .await;
                crate::CB.with(|cb| (cb.borrow().3)(ptr));
                result
            } else {
                f.await
            }
        });

        JoinHandle { fut: Some(fut) }
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

    #[derive(Debug, Copy, Clone)]
    pub struct JoinError;

    impl fmt::Display for JoinError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "JoinError")
        }
    }

    impl std::error::Error for JoinError {}

    pub struct JoinHandle<T> {
        fut: Option<compio_runtime::JoinHandle<T>>,
    }

    impl<T> JoinHandle<T> {
        pub fn is_finished(&self) -> bool {
            if let Some(hnd) = &self.fut {
                hnd.is_finished()
            } else {
                true
            }
        }
    }

    impl<T> Drop for JoinHandle<T> {
        fn drop(&mut self) {
            self.fut.take().unwrap().detach();
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(
                ready!(Pin::new(self.fut.as_mut().unwrap()).poll(cx))
                    .map_err(|_| JoinError),
            )
        }
    }
}

#[allow(dead_code)]
#[cfg(feature = "default-rt")]
mod default_rt {
    use std::task::{ready, Context, Poll};
    use std::{fmt, future::poll_fn, future::Future, pin::Pin};

    use ntex_runtime::Runtime;

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        log::info!(
            "Starting compio runtime, driver {:?}",
            ntex_iodriver::DriverType::current()
        );
        let rt = Runtime::new().unwrap();
        rt.block_on(fut);
    }

    /// Spawns a blocking task.
    ///
    /// The task will be spawned onto a thread pool specifically dedicated
    /// to blocking tasks. This is useful to prevent long-running synchronous
    /// operations from blocking the main futures executor.
    pub fn spawn_blocking<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + Sync + 'static,
        T: Send + 'static,
    {
        JoinHandle {
            fut: Some(ntex_runtime::spawn_blocking(f)),
        }
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
        let fut = ntex_runtime::spawn(async move {
            if let Some(ptr) = ptr {
                let mut f = std::pin::pin!(f);
                let result = poll_fn(|ctx| {
                    let new_ptr = crate::CB.with(|cb| (cb.borrow().1)(ptr));
                    let result = f.as_mut().poll(ctx);
                    crate::CB.with(|cb| (cb.borrow().2)(new_ptr));
                    result
                })
                .await;
                crate::CB.with(|cb| (cb.borrow().3)(ptr));
                result
            } else {
                f.await
            }
        });

        JoinHandle { fut: Some(fut) }
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

    #[derive(Debug, Copy, Clone)]
    pub struct JoinError;

    impl fmt::Display for JoinError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "JoinError")
        }
    }

    impl std::error::Error for JoinError {}

    pub struct JoinHandle<T> {
        fut: Option<ntex_runtime::JoinHandle<T>>,
    }

    impl<T> JoinHandle<T> {
        pub fn is_finished(&self) -> bool {
            if let Some(hnd) = &self.fut {
                hnd.is_finished()
            } else {
                true
            }
        }
    }

    impl<T> Drop for JoinHandle<T> {
        fn drop(&mut self) {
            self.fut.take().unwrap().detach();
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(
                ready!(Pin::new(self.fut.as_mut().unwrap()).poll(cx))
                    .map_err(|_| JoinError),
            )
        }
    }
}

#[allow(dead_code)]
#[cfg(feature = "async-std")]
mod asyncstd {
    use std::future::{poll_fn, Future};
    use std::{fmt, pin::Pin, task::ready, task::Context, task::Poll};

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
    pub fn spawn<F>(mut f: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
    {
        let ptr = crate::CB.with(|cb| (cb.borrow().0)());
        JoinHandle {
            fut: async_std::task::spawn_local(async move {
                if let Some(ptr) = ptr {
                    let mut f = unsafe { Pin::new_unchecked(&mut f) };
                    let result = poll_fn(|ctx| {
                        let new_ptr = crate::CB.with(|cb| (cb.borrow().1)(ptr));
                        let result = f.as_mut().poll(ctx);
                        crate::CB.with(|cb| (cb.borrow().2)(new_ptr));
                        result
                    })
                    .await;
                    crate::CB.with(|cb| (cb.borrow().3)(ptr));
                    result
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

#[allow(dead_code)]
#[cfg(all(feature = "glommio", target_os = "linux"))]
mod glommio {
    use std::future::{poll_fn, Future};
    use std::{pin::Pin, task::Context, task::Poll};

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
    pub fn spawn<F>(mut f: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let ptr = crate::CB.with(|cb| (cb.borrow().0)());
        JoinHandle {
            fut: Either::Left(
                glomm_io::spawn_local(async move {
                    if let Some(ptr) = ptr {
                        glomm_io::executor().yield_now().await;
                        let mut f = unsafe { Pin::new_unchecked(&mut f) };
                        let result = poll_fn(|ctx| {
                            let new_ptr = crate::CB.with(|cb| (cb.borrow().1)(ptr));
                            let result = f.as_mut().poll(ctx);
                            crate::CB.with(|cb| (cb.borrow().2)(new_ptr));
                            result
                        })
                        .await;
                        crate::CB.with(|cb| (cb.borrow().3)(ptr));
                        result
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
    #[allow(clippy::type_complexity)]
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
pub use self::tokio::*;

#[cfg(feature = "async-std")]
pub use self::asyncstd::*;

#[cfg(feature = "glommio")]
pub use self::glommio::*;

#[cfg(feature = "compio")]
pub use self::compio::*;

#[cfg(feature = "default-rt")]
pub use self::default_rt::*;

#[allow(dead_code)]
#[cfg(all(
    not(feature = "tokio"),
    not(feature = "async-std"),
    not(feature = "compio"),
    not(feature = "default-rt"),
    not(feature = "glommio")
))]
mod no_rt {
    use std::task::{Context, Poll};
    use std::{fmt, future::Future, marker::PhantomData, pin::Pin};

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(_: F) {
        panic!("async runtime is not configured");
    }

    pub fn spawn<F>(_: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
    {
        unimplemented!()
    }

    pub fn spawn_blocking<F, T>(_: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + Sync + 'static,
        T: Send + 'static,
    {
        unimplemented!()
    }

    /// Blocking operation completion future. It resolves with results
    /// of blocking function execution.
    #[allow(clippy::type_complexity)]
    pub struct JoinHandle<T> {
        t: PhantomData<T>,
    }

    impl<T> JoinHandle<T> {
        pub fn is_finished(&self) -> bool {
            true
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            todo!()
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
}

#[cfg(all(
    not(feature = "tokio"),
    not(feature = "async-std"),
    not(feature = "compio"),
    not(feature = "default-rt"),
    not(feature = "glommio")
))]
pub use self::no_rt::*;
