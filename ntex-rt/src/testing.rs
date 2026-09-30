//! Test helpers.
use std::any::Any;

use crate::{BlockFuture, Runner};

#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
mod busy {
    use std::io;

    use crate::{Driver, Notify, PollResult, Runtime};

    #[derive(Debug)]
    pub(super) struct NoopNotify;

    impl Notify for NoopNotify {
        fn notify(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Driver that polls the runtime in a busy loop.
    pub(super) struct BusyDriver;

    impl Driver for BusyDriver {
        fn handle(&self) -> Box<dyn Notify> {
            Box::new(NoopNotify)
        }

        fn run(&self, rt: &Runtime) -> io::Result<()> {
            while rt.poll() != PollResult::Ready {}
            Ok(())
        }
    }
}

/// Runner for tests that do not need I/O or timers.
///
/// Uses the runtime `crate::spawn()` spawns on for the enabled features.
pub(crate) struct TestRunner;

impl Runner for TestRunner {
    fn block_on(&self, fut: BlockFuture) -> Result<(), Box<dyn Any + Send>> {
        #[cfg(feature = "tokio")]
        {
            let rt = tok_io::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            tok_io::task::LocalSet::new().block_on(&rt, fut);
        }

        #[cfg(all(feature = "compio", not(feature = "tokio")))]
        compio_runtime::Runtime::new().unwrap().block_on(fut);

        #[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
        crate::Runtime::new(Box::new(busy::NoopNotify)).block_on(fut, &busy::BusyDriver);

        Ok(())
    }
}
