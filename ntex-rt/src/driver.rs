//! The platform-specified driver.
use std::{fmt, io, pin::Pin};

use crate::rt::Runtime;

pub type BlockFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DriverType {
    Poll,
    IoUring,
}

impl DriverType {
    pub const fn name(&self) -> &'static str {
        match self {
            DriverType::Poll => "polling",
            DriverType::IoUring => "io-uring",
        }
    }

    pub const fn is_polling(&self) -> bool {
        matches!(self, &DriverType::Poll)
    }
}

pub trait Runner: Send + Sync + 'static {
    fn block_on(&self, fut: BlockFuture);
}

pub trait Driver: 'static {
    fn handle(&self) -> Box<dyn Notify>;

    fn run(&self, rt: &Runtime) -> io::Result<()>;
}

pub enum PollResult {
    Ready,
    Pending,
    PollAgain,
}

pub trait Notify: Send + Sync + fmt::Debug + 'static {
    fn notify(&self) -> io::Result<()>;
}
