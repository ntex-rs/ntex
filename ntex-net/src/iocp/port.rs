use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle, RawHandle};

// use crate::{Overlapped, RawFd};

pub(crate) struct Port {
    port: OwnedHandle,
}

impl Port {
    pub fn new() -> io::Result<Self> {
        let port = unsafe {
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1);
            if port.is_null() {
                return Err(io::Error::last_os_error());
            }
            OwnedHandle::from_raw_handle(port)
        };
        log::trace!("New iocp handle: {port:p}");
        Ok(Self { port })
    }

    pub fn attach(&self, fd: RawFd) -> io::Result<()> {
        self.port.attach(fd)
    }

    #[allow(dead_code)]
    pub fn post(&self, res: io::Result<usize>, optr: *mut Overlapped) -> io::Result<()> {
        self.port.post(res, optr)
    }

    pub fn post_raw(&self, optr: *const Overlapped) -> io::Result<()> {
        self.port.post_raw(optr)
    }

    pub fn poll(
        &self,
        timeout: Option<Duration>,
    ) -> io::Result<impl Iterator<Item = RawEntry> + '_> {
        let current_id = self.as_raw_handle() as _;
        self.port.poll(timeout, Some(current_id))
    }
}

impl AsRawHandle for Port {
    fn as_raw_handle(&self) -> RawHandle {
        self.port.as_raw_handle()
    }
}
