#![allow(clippy::type_complexity, clippy::let_underscore_future)]
//! A runtime implementation that runs everything on the current thread.
use std::{cell::Cell, ptr};

mod arbiter;
mod builder;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::system::{Id, PingRecord, System};

thread_local! {
    static CB: Cell<*const Callbacks> = const { Cell::new(ptr::null()) };
}

struct Callbacks {
    before: Box<dyn Fn() -> Option<*const ()>>,
    enter: Box<dyn Fn(*const ()) -> *const ()>,
    exit: Box<dyn Fn(*const ())>,
    after: Box<dyn Fn(*const ())>,
}

struct Data {
    cb: &'static Callbacks,
    ptr: *const (),
}

impl Data {
    fn load() -> Option<Data> {
        let cb = CB.with(|cb| cb.get());

        if let Some(cb) = unsafe { cb.as_ref() } {
            if let Some(ptr) = (*cb.before)() {
                return Some(Data { cb, ptr });
            }
        }
        None
    }

    fn run<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let ptr = (*self.cb.enter)(self.ptr);
        let result = f();
        (*self.cb.exit)(ptr);
        result
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        (*self.cb.after)(self.ptr)
    }
}

/// # Safety
///
/// The user must ensure that the pointer returned by `before` has a `'static` lifetime.
/// This pointer will be owned by the spawned task for the duration of that task, and
/// ownership will be returned to the user at the end of the task via `after`.
/// The pointer remains opaque to the runtime.
///
/// # Panics
///
/// Panics if spawn callbacks have already been set.
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
        if cb.get().is_null() {
            panic!("Spawn callbacks already set");
        }

        let new: *mut Callbacks = Box::leak(Box::new(Callbacks {
            before: Box::new(before),
            enter: Box::new(enter),
            exit: Box::new(exit),
            after: Box::new(after),
        }));
        cb.replace(new);
    });
}

pub unsafe fn spawn_cbs_try<FBefore, FEnter, FExit, FAfter>(
    before: FBefore,
    enter: FEnter,
    exit: FExit,
    after: FAfter,
) -> bool
where
    FBefore: Fn() -> Option<*const ()> + 'static,
    FEnter: Fn(*const ()) -> *const () + 'static,
    FExit: Fn(*const ()) + 'static,
    FAfter: Fn(*const ()) + 'static,
{
    CB.with(|cb| {
        if cb.get().is_null() {
            false
        } else {
            spawn_cbs(before, enter, exit, after);
            true
        }
    });
}

#[allow(dead_code)]
#[cfg(feature = "tokio")]
mod tokio {
    use std::future::{Future, poll_fn};
    pub use tok_io::task::{JoinError, JoinHandle, spawn_blocking};

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
        let data = crate::Data::load();
        if let Some(mut data) = data {
            tok_io::task::spawn_local(async move {
                tok_io::pin!(f);
                poll_fn(|cx| data.run(|| f.as_mut().poll(cx))).await
            })
        } else {
            tok_io::task::spawn_local(f)
        }
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
    use std::task::{Context, Poll, ready};
    use std::{fmt, future::Future, future::poll_fn, pin::Pin};

    use compio_driver::DriverType;
    use compio_runtime::Runtime;

    /// Runs the provided future, blocking the current thread until the future
    /// completes.
    pub fn block_on<F: Future<Output = ()>>(fut: F) {
        log::info!(
            "Starting compio runtime, driver {:?}",
            compio_runtime::Runtime::try_with_current(|rt| rt.driver_type())
                .unwrap_or(DriverType::Poll)
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
            fut: Some(Either::Compio(compio_runtime::spawn_blocking(f))),
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
        let fut = if let Some(mut data) = crate::Data::load() {
            compio_runtime::spawn(async move {
                let mut f = std::pin::pin!(f);
                poll_fn(|cx| data.run(|| f.as_mut().poll(cx))).await
            })
        } else {
            compio_runtime::spawn(f)
        };

        JoinHandle {
            fut: Some(Either::Compio(fut)),
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

    enum Either<T> {
        Compio(compio_runtime::JoinHandle<T>),
        Spawn(oneshot::Receiver<T>),
    }

    pub struct JoinHandle<T> {
        fut: Option<Either<T>>,
    }

    impl<T> JoinHandle<T> {
        pub fn is_finished(&self) -> bool {
            match &self.fut {
                Some(Either::Compio(fut)) => fut.is_finished(),
                Some(Either::Spawn(fut)) => fut.is_closed(),
                None => true,
            }
        }
    }

    impl<T> Drop for JoinHandle<T> {
        fn drop(&mut self) {
            if let Some(Either::Compio(fut)) = self.fut.take() {
                fut.detach();
            }
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(match self.fut.as_mut() {
                Some(Either::Compio(fut)) => {
                    ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError)
                }
                Some(Either::Spawn(fut)) => {
                    ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError)
                }
                None => Err(JoinError),
            })
        }
    }

    #[derive(Clone, Debug)]
    pub struct Handle(crate::Arbiter);

    impl Handle {
        pub fn current() -> Self {
            Self(crate::Arbiter::current())
        }

        pub fn notify(&self) {}

        pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            let (tx, rx) = oneshot::channel();
            self.0.spawn(async move {
                let result = future.await;
                let _ = tx.send(result);
            });
            JoinHandle {
                fut: Some(Either::Spawn(rx)),
            }
        }
    }
}

#[allow(dead_code)]
#[cfg(feature = "neon")]
mod neon {
    use std::task::{Context, Poll, ready};
    use std::{fmt, future::Future, future::poll_fn, pin::Pin};

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
        let task = if let Some(mut data) = crate::Data::load() {
            ntex_neon::spawn(async move {
                let mut f = std::pin::pin!(f);
                poll_fn(|cx| data.run(|| f.as_mut().poll(cx))).await
            })
        } else {
            ntex_neon::spawn(f)
        };

        JoinHandle {
            task: Some(Either::Task(task)),
        }
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
            task: Some(Either::Blocking(ntex_neon::spawn_blocking(f))),
        }
    }

    #[derive(Clone, Debug)]
    pub struct Handle(ntex_neon::Handle);

    impl Handle {
        pub fn current() -> Self {
            Self(ntex_neon::Handle::current())
        }

        pub fn notify(&self) {
            let _ = self.0.notify();
        }

        pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            JoinHandle {
                task: Some(Either::Task(self.0.spawn(future))),
            }
        }
    }

    enum Either<T> {
        Task(ntex_neon::Task<T>),
        Blocking(ntex_neon::JoinHandle<T>),
    }

    /// A spawned task.
    pub struct JoinHandle<T> {
        task: Option<Either<T>>,
    }

    impl<T> JoinHandle<T> {
        pub fn is_finished(&self) -> bool {
            match &self.task {
                Some(Either::Task(fut)) => fut.is_finished(),
                Some(Either::Blocking(fut)) => fut.is_closed(),
                None => true,
            }
        }
    }

    impl<T> Drop for JoinHandle<T> {
        fn drop(&mut self) {
            if let Some(Either::Task(fut)) = self.task.take() {
                fut.detach();
            }
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(match self.task.as_mut() {
                Some(Either::Task(fut)) => Ok(ready!(Pin::new(fut).poll(cx))),
                Some(Either::Blocking(fut)) => ready!(Pin::new(fut).poll(cx))
                    .map_err(|_| JoinError)
                    .and_then(|res| res.map_err(|_| JoinError)),
                None => Err(JoinError),
            })
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
