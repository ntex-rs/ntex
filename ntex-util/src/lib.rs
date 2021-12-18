//! Utilities for ntex framework
use std::{cell::RefCell, future::Future, pin::Pin};

pub mod channel;
pub mod future;
pub mod task;
pub mod time;

pub use futures_core::{ready, Stream};
pub use futures_sink::Sink;

pub type HashMap<K, V> = std::collections::HashMap<K, V, fxhash::FxBuildHasher>;
pub type HashSet<V> = std::collections::HashSet<V, fxhash::FxBuildHasher>;

thread_local! {
    #[allow(clippy::type_complexity)]
    static SPAWNER: RefCell<Box<dyn Fn(Pin<Box<dyn Future<Output = ()>>>)>> = RefCell::new(Box::new(|_| {
        panic!("spawn fn is not configured");
    }));
}

/// Spawn a future on the current thread.
///
/// # Panics
///
/// This function panics if spawn fn is not set.
#[inline]
pub fn spawn<F>(fut: F)
where
    F: Future<Output = ()> + 'static,
{
    SPAWNER.with(move |f| {
        (*f.borrow())(Box::pin(fut));
    });
}

pub fn set_spawn_fn<F>(f: F)
where
    F: Fn(Pin<Box<dyn Future<Output = ()>>>) + 'static,
{
    SPAWNER.with(|ctx| {
        *ctx.borrow_mut() = Box::new(f);
    });
}
