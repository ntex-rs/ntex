#![allow(clippy::missing_safety_doc)]
use std::{future::Future, io, mem, mem::MaybeUninit};

use ntex_iodriver::{impl_raw_fd, op::CloseSocket, op::ShutdownSocket, syscall, AsRawFd};
use socket2::{Domain, Protocol, SockAddr, Socket as Socket2, Type};

#[derive(Debug)]
pub struct Socket {
    socket: Socket2,
}

impl Socket {
    pub fn from_socket2(socket: Socket2) -> io::Result<Self> {
        #[cfg(unix)]
        {
            #[cfg(not(any(
                target_os = "android",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "fuchsia",
                target_os = "hurd",
                target_os = "illumos",
                target_os = "linux",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "espidf",
                target_os = "vita",
            )))]
            socket.set_cloexec(true)?;
            #[cfg(any(
                target_os = "ios",
                target_os = "macos",
                target_os = "tvos",
                target_os = "watchos",
            ))]
            socket.set_nosigpipe(true)?;

            // On Linux we use blocking socket
            // Newer kernels have the patch that allows to arm io_uring poll mechanism for
            // non blocking socket when there is no connections in listen queue
            //
            // https://patchwork.kernel.org/project/linux-block/patch/f999615b-205c-49b7-b272-c4e42e45e09d@kernel.dk/#22949861
            if cfg!(not(all(target_os = "linux", feature = "io-uring")))
                || ntex_iodriver::DriverType::is_polling()
            {
                socket.set_nonblocking(true)?;
            }
        }

        Ok(Self { socket })
    }

    pub fn peer_addr(&self) -> io::Result<SockAddr> {
        self.socket.peer_addr()
    }

    pub fn local_addr(&self) -> io::Result<SockAddr> {
        self.socket.local_addr()
    }

    #[cfg(windows)]
    pub async fn new(
        domain: Domain,
        ty: Type,
        protocol: Option<Protocol>,
    ) -> io::Result<Self> {
        use std::panic::resume_unwind;

        let socket = crate::spawn_blocking(move || Socket2::new(domain, ty, protocol))
            .await
            .unwrap_or_else(|e| resume_unwind(e))?;
        Self::from_socket2(socket)
    }

    #[cfg(unix)]
    pub async fn new(
        domain: Domain,
        ty: Type,
        protocol: Option<Protocol>,
    ) -> io::Result<Self> {
        use std::os::fd::FromRawFd;

        #[allow(unused_mut)]
        let mut ty: i32 = ty.into();
        #[cfg(any(
            target_os = "android",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "fuchsia",
            target_os = "hurd",
            target_os = "illumos",
            target_os = "linux",
            target_os = "netbsd",
            target_os = "openbsd",
        ))]
        {
            ty |= libc::SOCK_CLOEXEC;
        }

        let op = ntex_iodriver::op::CreateSocket::new(
            domain.into(),
            ty,
            protocol.map(|p| p.into()).unwrap_or_default(),
        );
        let (res, _) = crate::submit(op).await;
        let socket = unsafe { Socket2::from_raw_fd(res? as _) };

        Self::from_socket2(socket)
    }

    pub async fn bind(
        addr: &SockAddr,
        ty: Type,
        protocol: Option<Protocol>,
    ) -> io::Result<Self> {
        let socket = Self::new(addr.domain(), ty, protocol).await?;
        socket.socket.bind(addr)?;
        Ok(socket)
    }

    pub fn listen(&self, backlog: i32) -> io::Result<()> {
        self.socket.listen(backlog)
    }

    pub fn close(self) -> impl Future<Output = io::Result<()>> {
        let op = CloseSocket::from_raw_fd(self.as_raw_fd());
        let fut = crate::submit(op);
        mem::forget(self);
        async move {
            fut.await.0?;
            Ok(())
        }
    }

    pub async fn shutdown(&self) -> io::Result<()> {
        let op = ShutdownSocket::new(self.as_raw_fd(), std::net::Shutdown::Write);
        crate::submit(op).await.0?;
        Ok(())
    }

    #[cfg(unix)]
    pub unsafe fn get_socket_option<T: Copy>(
        &self,
        level: i32,
        name: i32,
    ) -> io::Result<T> {
        let mut value: MaybeUninit<T> = MaybeUninit::uninit();
        let mut len = size_of::<T>() as libc::socklen_t;
        syscall!(libc::getsockopt(
            self.socket.as_raw_fd(),
            level,
            name,
            value.as_mut_ptr() as _,
            &mut len
        ))
        .map(|_| {
            debug_assert_eq!(len as usize, size_of::<T>());
            // SAFETY: The value is initialized by `getsockopt`.
            value.assume_init()
        })
    }

    #[cfg(windows)]
    pub unsafe fn get_socket_option<T: Copy>(
        &self,
        level: i32,
        name: i32,
    ) -> io::Result<T> {
        let mut value: MaybeUninit<T> = MaybeUninit::uninit();
        let mut len = size_of::<T>() as i32;
        syscall!(
            SOCKET,
            windows_sys::Win32::Networking::WinSock::getsockopt(
                self.socket.as_raw_fd() as _,
                level,
                name,
                value.as_mut_ptr() as _,
                &mut len
            )
        )
        .map(|_| {
            debug_assert_eq!(len as usize, size_of::<T>());
            // SAFETY: The value is initialized by `getsockopt`.
            value.assume_init()
        })
    }

    #[cfg(unix)]
    pub unsafe fn set_socket_option<T: Copy>(
        &self,
        level: i32,
        name: i32,
        value: &T,
    ) -> io::Result<()> {
        syscall!(libc::setsockopt(
            self.socket.as_raw_fd(),
            level,
            name,
            value as *const _ as _,
            std::mem::size_of::<T>() as _
        ))
        .map(|_| ())
    }

    #[cfg(windows)]
    pub unsafe fn set_socket_option<T: Copy>(
        &self,
        level: i32,
        name: i32,
        value: &T,
    ) -> io::Result<()> {
        syscall!(
            SOCKET,
            windows_sys::Win32::Networking::WinSock::setsockopt(
                self.socket.as_raw_fd() as _,
                level,
                name,
                value as *const _ as _,
                std::mem::size_of::<T>() as _
            )
        )
        .map(|_| ())
    }
}

impl_raw_fd!(Socket, Socket2, socket, socket);
