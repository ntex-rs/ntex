//! A runtime implementation that runs everything on the current thread.

pub use actix_macros::{main, test};

mod arbiter;
mod builder;
mod runtime;
mod system;

pub use self::arbiter::Arbiter;
pub use self::builder::{Builder, SystemRunner};
pub use self::runtime::Runtime;
pub use self::system::System;

#[doc(hidden)]
pub use actix_threadpool as blocking;

/// Spawns a future on the current arbiter.
///
/// # Panics
///
/// This function panics if actix system is not running.
pub fn spawn<F>(f: F)
where
    F: futures::Future<Output = ()> + 'static,
{
    if !System::is_set() {
        panic!("System is not running");
    }

    Arbiter::spawn(f);
}
