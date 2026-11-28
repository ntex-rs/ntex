//! Test helpers.
use std::any::Any;

use crate::{BlockFuture, Runner};

#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
mod busy {
    use std::io;

    use crate::rt::Runtime;
    use crate::{Driver, Notify, PollResult};

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

    /// Unparks the driver thread.
    #[derive(Debug)]
    struct ParkNotify(std::thread::Thread);

    impl Notify for ParkNotify {
        fn notify(&self) -> io::Result<()> {
            self.0.unpark();
            Ok(())
        }
    }

    /// Driver that parks the thread while the runtime is idle.
    pub(super) struct ParkDriver;

    impl Driver for ParkDriver {
        fn handle(&self) -> Box<dyn Notify> {
            Box::new(ParkNotify(std::thread::current()))
        }

        fn run(&self, rt: &Runtime) -> io::Result<()> {
            loop {
                match rt.poll() {
                    PollResult::Ready => return Ok(()),
                    PollResult::PollAgain => continue,
                    PollResult::Pending(None) => std::thread::park(),
                    PollResult::Pending(Some(timeout)) => std::thread::park_timeout(timeout),
                }
                rt.awake();
            }
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
                .enable_time()
                .build()
                .unwrap();
            tok_io::task::LocalSet::new().block_on(&rt, fut);
        }

        #[cfg(all(feature = "compio", not(feature = "tokio")))]
        compio_runtime::Runtime::new().unwrap().block_on(fut);

        #[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
        crate::rt::Runtime::new(Box::new(busy::NoopNotify)).block_on(fut, &busy::BusyDriver);

        Ok(())
    }
}

/// Runner for the native runtime whose driver blocks while the runtime is idle.
#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
pub(crate) struct ParkRunner;

#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
impl Runner for ParkRunner {
    fn block_on(&self, fut: BlockFuture) -> Result<(), Box<dyn Any + Send>> {
        let driver = busy::ParkDriver;
        crate::rt::Runtime::new(crate::Driver::handle(&driver)).block_on(fut, &driver);
        Ok(())
    }
}
