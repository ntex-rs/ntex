//! Test helpers.
use std::{any::Any, io};

use crate::{BlockFuture, Driver, Notify, PollResult, Runner, Runtime};

#[derive(Debug)]
struct NoopNotify;

impl Notify for NoopNotify {
    fn notify(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Driver that polls the runtime in a busy loop.
struct BusyDriver;

impl Driver for BusyDriver {
    fn handle(&self) -> Box<dyn Notify> {
        Box::new(NoopNotify)
    }

    fn run(&self, rt: &Runtime) -> io::Result<()> {
        while rt.poll() != PollResult::Ready {}
        Ok(())
    }
}

/// Runner for tests that do not need I/O or timers.
pub(crate) struct TestRunner;

impl Runner for TestRunner {
    fn block_on(&self, fut: BlockFuture) -> Result<(), Box<dyn Any + Send>> {
        Runtime::new(Box::new(NoopNotify)).block_on(fut, &BusyDriver);
        Ok(())
    }
}
