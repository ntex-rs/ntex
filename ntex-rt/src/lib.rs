#![allow(clippy::type_complexity, clippy::let_underscore_future)]
//! A runtime implementation that runs everything on the current thread.
use std::{cell::RefCell, ptr};

mod arbiter;
mod builder;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::system::{Id, PingRecord, System};

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
    pub use tok_io::task::{spawn_blocking, JoinError, JoinHandle};

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        if let Ok(hnd) = tok_io::runtime::Handle::try_current() {
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
    #[doc(hidden)]
    #[deprecated]
    pub fn spawn_fn<F, R>(f: F) -> tok_io::task::JoinHandle<R::Output>
    where
        F: FnOnce() -> R + 'static,
        R: Future + 'static,
    {
        spawn(async move { f().await })
    }

    #[derive(Clone, Debug)]
    /// Handle to the runtime.
    pub struct Handle(tok_io::runtime::Handle);

    impl Handle {
        #[inline]
        pub fn current() -> Self {
            Self(tok_io::runtime::Handle::current())
        }

        #[inline]
        /// Wake up runtime
        pub fn notify(&self) {}

        #[inline]
        /// Spawns a new asynchronous task, returning a [`Task`] for it.
        ///
        /// Spawning a task enables the task to execute concurrently to other tasks.
        /// There is no guarantee that a spawned task will execute to completion.
        pub fn spawn<F>(&self, future: F) -> tok_io::task::JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            self.0.spawn(future)
        }
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
    #[doc(hidden)]
    #[deprecated]
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

    #[derive(Clone, Debug)]
    pub struct Handle(crate::Arbiter);

    impl Handle {
        pub fn current() -> Self {
            Self(crate::Arbiter::current())
        }

        pub fn notify(&self) {}

        pub fn spawn<F>(&self, future: F) -> impl Future<Output = F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            let (tx, rx) = oneshot::channel();
            self.0.spawn(async move {
                let result = future.await;
                let _ = tx.send(result);
            });
            async { rx.await.unwrap() }
        }
    }
}

#[allow(dead_code)]
#[cfg(feature = "neon")]
mod neon {
    use std::task::{ready, Context, Poll};
    use std::{fmt, future::poll_fn, future::Future, pin::Pin};

    pub use ntex_neon::Handle;
    use ntex_neon::Runtime;

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        let rt = Runtime::new().unwrap();
        log::info!(
            "Starting neon runtime, driver {:?}",
            rt.driver_type().name()
        );

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
            fut: Some(ntex_neon::spawn_blocking(f)),
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
    pub fn spawn<F>(f: F) -> Task<F::Output>
    where
        F: Future + 'static,
    {
        let ptr = crate::CB.with(|cb| (cb.borrow().0)());
        let task = ntex_neon::spawn(async move {
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

        Task { task: Some(task) }
    }

    /// Executes a future on the current thread. This does not create a new Arbiter
    /// or Arbiter address, it is simply a helper for executing futures on the current
    /// thread.
    ///
    /// # Panics
    ///
    /// This function panics if ntex system is not running.
    #[inline]
    #[doc(hidden)]
    #[deprecated]
    pub fn spawn_fn<F, R>(f: F) -> Task<R::Output>
    where
        F: FnOnce() -> R + 'static,
        R: Future + 'static,
    {
        spawn(async move { f().await })
    }

    /// A spawned task.
    pub struct Task<T> {
        task: Option<ntex_neon::Task<T>>,
    }

    impl<T> Task<T> {
        pub fn is_finished(&self) -> bool {
            if let Some(hnd) = &self.task {
                hnd.is_finished()
            } else {
                true
            }
        }
    }

    impl<T> Drop for Task<T> {
        fn drop(&mut self) {
            self.task.take().unwrap().detach();
        }
    }

    impl<T> Future for Task<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(Ok(ready!(Pin::new(self.task.as_mut().unwrap()).poll(cx))))
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
        fut: Option<ntex_neon::JoinHandle<T>>,
    }

    impl<T> JoinHandle<T> {
        pub fn is_finished(&self) -> bool {
            self.fut.is_none()
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(
                ready!(Pin::new(self.fut.as_mut().unwrap()).poll(cx))
                    .map_err(|_| JoinError)
                    .and_then(|result| result.map_err(|_| JoinError)),
            )
        }
    }
}

#[cfg(feature = "tokio")]
pub use self::tokio::*;

#[cfg(feature = "compio")]
pub use self::compio::*;

#[cfg(feature = "neon")]
pub use self::neon::*;

#[allow(dead_code)]
#[cfg(all(not(feature = "tokio"), not(feature = "compio"), not(feature = "neon")))]
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

    #[derive(Clone, Debug)]
    /// Handle to the runtime.
    pub struct Handle;

    impl Handle {
        #[inline]
        pub fn current() -> Self {
            Self
        }

        #[inline]
        /// Wake up runtime
        pub fn notify(&self) {}

        #[inline]
        /// Spawns a new asynchronous task, returning a [`Task`] for it.
        ///
        /// Spawning a task enables the task to execute concurrently to other tasks.
        /// There is no guarantee that a spawned task will execute to completion.
        pub fn spawn<F>(&self, _: F) -> JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            panic!("async runtime is not configured");
        }
    }
}

#[cfg(all(not(feature = "tokio"), not(feature = "compio"), not(feature = "neon")))]
pub use self::no_rt::*;
