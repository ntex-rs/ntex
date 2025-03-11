use std::sync::atomic::{AtomicU8, Ordering};

const UNINIT: u8 = u8::MAX;
const IO_URING: u8 = 0;
const POLLING: u8 = 1;
const IOCP: u8 = 2;

static DRIVER_TYPE: AtomicU8 = AtomicU8::new(UNINIT);

/// Representing underlying driver type the fusion driver is using
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverType {
    /// Using `polling` driver
    Poll = POLLING,

    /// Using `io-uring` driver
    IoUring = IO_URING,

    /// Using `iocp` driver
    IOCP = IOCP,
}

impl DriverType {
    fn from_num(n: u8) -> Self {
        match n {
            IO_URING => Self::IoUring,
            POLLING => Self::Poll,
            IOCP => Self::IOCP,
            _ => unreachable!("invalid driver type"),
        }
    }

    /// Get the underlying driver type
    fn get() -> DriverType {
        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                DriverType::IOCP
            } else if #[cfg(all(target_os = "linux", feature = "polling", feature = "io-uring"))] {
                use io_uring::opcode::*;

                // Add more opcodes here if used
                const USED_OP: &[u8] = &[
                    Read::CODE,
                    Readv::CODE,
                    Write::CODE,
                    Writev::CODE,
                    Fsync::CODE,
                    Accept::CODE,
                    Connect::CODE,
                    RecvMsg::CODE,
                    SendMsg::CODE,
                    AsyncCancel::CODE,
                    OpenAt::CODE,
                    Close::CODE,
                    Shutdown::CODE,
                ];

                (|| {
                    let uring = io_uring::IoUring::new(2)?;
                    let mut probe = io_uring::Probe::new();
                    uring.submitter().register_probe(&mut probe)?;
                    if USED_OP.iter().all(|op| probe.is_supported(*op)) {
                        std::io::Result::Ok(DriverType::IoUring)
                    } else {
                        Ok(DriverType::Poll)
                    }
                })()
                .unwrap_or(DriverType::Poll) // Should we fail here?
            } else if #[cfg(all(target_os = "linux", feature = "io-uring"))] {
                DriverType::IoUring
            } else if #[cfg(unix)] {
                DriverType::Poll
            } else {
                compile_error!("unsupported platform");
            }
        }
    }

    /// Get the underlying driver type and cache it. Following calls will return
    /// the cached value.
    pub fn current() -> DriverType {
        match DRIVER_TYPE.load(Ordering::Acquire) {
            UNINIT => {}
            x => return DriverType::from_num(x),
        }
        let dev_ty = Self::get();

        DRIVER_TYPE.store(dev_ty as u8, Ordering::Release);

        dev_ty
    }

    /// Check if the current driver is `polling`
    pub fn is_polling() -> bool {
        Self::current() == DriverType::Poll
    }

    /// Check if the current driver is `io-uring`
    pub fn is_iouring() -> bool {
        Self::current() == DriverType::IoUring
    }

    /// Check if the current driver is `iocp`
    pub fn is_iocp() -> bool {
        Self::current() == DriverType::IOCP
    }
}
