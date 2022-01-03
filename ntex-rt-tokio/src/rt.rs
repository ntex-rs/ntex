use std::{future::Future, io, pin::Pin};

use tokio::{runtime, task::LocalSet};

/// Runs the provided future, blocking the current thread until the future
/// completes.
pub fn block_on<F: Future<Output = ()>>(fut: F) {
    TokioRuntime::new().unwrap().block_on(fut)
}

/// Single-threaded tokio runtime.
#[derive(Debug)]
struct TokioRuntime {
    local: LocalSet,
    rt: runtime::Runtime,
}
impl TokioRuntime {
    fn new() -> io::Result<Self> {
        let rt = runtime::Builder::new_current_thread().enable_io().build()?;

        Ok(Self {
            rt,
            local: LocalSet::new(),
        })
    }

    fn block_on<F: Future<Output = ()>>(&self, f: F) {
        // set ntex-util spawn fn
        ntex_util::set_spawn_fn(|fut| {
            tokio::task::spawn_local(fut);
        });

        self.local.block_on(&self.rt, f);
    }
}
