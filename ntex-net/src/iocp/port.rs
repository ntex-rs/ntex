use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::{fmt, io, ptr};

use windows_sys::Win32::{
    Foundation::{
        ERROR_BAD_COMMAND, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE,
        ERROR_MORE_DATA, ERROR_NETNAME_DELETED, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_NOT_CONNECTED, FACILITY_NTWIN32, INVALID_HANDLE_VALUE, NTSTATUS,
        RtlNtStatusToDosError, STATUS_SUCCESS,
    },
    Storage::FileSystem::SetFileCompletionNotificationModes,
    System::{
        IO::{
            CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
            PostQueuedCompletionStatus,
        },
        SystemServices::ERROR_SEVERITY_ERROR,
        Threading::INFINITE,
        WindowsProgramming::{
            FILE_SKIP_COMPLETION_PORT_ON_SUCCESS, FILE_SKIP_SET_EVENT_ON_HANDLE,
        },
    },
};

use super::Overlapped;

pub(crate) struct Port {
    port: OwnedHandle,
}

impl Port {
    pub fn new() -> io::Result<Self> {
        let port = unsafe {
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1);
            if port.is_null() {
                return Err(io::Error::last_os_error());
            }
            OwnedHandle::from_raw_handle(port)
        };
        // log::trace!("New iocp handle: {port:p}");
        Ok(Self { port })
    }

    pub fn attach(&self, fd: RawHandle) -> io::Result<()> {
        // self.port.attach(fd)
        todo!()
    }

    #[allow(dead_code)]
    pub fn post(&self, res: io::Result<usize>, optr: *mut Overlapped) -> io::Result<()> {
        // self.port.post(res, optr)
        todo!()
    }

    pub fn post_raw(&self, optr: *const Overlapped) -> io::Result<()> {
        // self.port.post_raw(optr)
        todo!()
    }

    pub fn poll(&self) -> io::Result<impl Iterator<Item = Overlapped> + '_> {
        // let current_id = self.as_raw_handle() as _;
        // self.port.poll(Some(current_id))
        todo!()
    }
}

impl AsRawHandle for Port {
    #[inline]
    fn as_raw_handle(&self) -> RawHandle {
        self.port.as_raw_handle()
    }
}
