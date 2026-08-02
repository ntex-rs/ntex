use std::{cell::Cell, io, mem, os::windows::io::AsRawSocket, rc::Rc};

use ntex_io::IoContext;
use ntex_rt::{Arbiter, syscall};
use ntex_util::{channel::pool, future::Either};
use slab::Slab;
use socket2::Socket;
use windows_sys::Win32::Networking::WinSock;

use super::{Driver, DriverApi, Handler, Overlapped, ops};

#[derive(Clone)]
pub(crate) struct StreamOps(Rc<StreamOpsInner>);

pub(crate) struct StreamCtl {
    id: usize,
    inner: Rc<StreamOpsInner>,
}

pub(crate) struct WeakStreamCtl {
    id: usize,
    inner: Rc<StreamOpsInner>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const CLOSED       = 0b0010_0000;
        const DROPPED_PRI  = 0b0100_0000;
        const DROPPED_SEC  = 0b1000_0000;
    }
}

#[derive(Debug)]
struct StreamItem {
    io: Socket,
    flags: Flags,
    rd_op: ops::ReadOperation,
    wr_op: ops::WriteOperation,
    close: Option<pool::Sender<io::Result<()>>>,
}

struct StreamOpsHandler {
    inner: Rc<StreamOpsInner>,
}

#[allow(clippy::box_collection)]
struct StreamOpsInner {
    api: DriverApi,
    storage: Cell<Option<Box<StreamOpsStorage>>>,
    pool: pool::Pool<io::Result<()>>,
}

struct StreamOpsStorage {
    streams: Slab<Box<StreamItem>>,
}

impl StreamOps {
    /// Get `StreamOps` instance from the current runtime, or create new one
    pub(crate) fn get(driver: &Driver) -> Self {
        Arbiter::get_value(|| {
            let mut inner = None;
            driver.register(|api| {
                let ops = Rc::new(StreamOpsInner {
                    api,
                    pool: pool::new(),
                    storage: Cell::new(Some(Box::new(StreamOpsStorage {
                        streams: Slab::new(),
                    }))),
                });
                inner = Some(ops.clone());
                Box::new(StreamOpsHandler { inner: ops })
            });

            StreamOps(inner.unwrap())
        })
    }

    pub(crate) fn register(self, io: Socket, ctx: IoContext) -> (StreamCtl, WeakStreamCtl) {
        let sock = io.as_raw_socket();
        #[cfg(feature = "trace")]
        log::trace!("{}: Registered({:?})", ctx.tag(), sock);

        let mut storage = self.0.storage.take().unwrap();
        let entry = storage.streams.vacant_entry();
        let id = entry.key();

        // read op
        let rd_op = ops::ReadOperation::new(id, sock, ctx.clone(), &self.0.api);

        // write op
        let wr_op = ops::WriteOperation::new(id, sock, ctx, &self.0.api);

        entry.insert(Box::new(StreamItem {
            io,
            rd_op,
            wr_op,
            close: None,
            flags: Flags::empty(),
        }));
        self.0.storage.set(Some(storage));

        (
            StreamCtl {
                id,
                inner: self.0.clone(),
            },
            WeakStreamCtl {
                id,
                inner: self.0.clone(),
            },
        )
    }
}

impl Handler for StreamOpsHandler {
    fn completed(&mut self, udata: u32, res: io::Result<usize>, optr: *mut Overlapped) {
        if let Some(id) = match udata {
            ops::RD_OP => ops::ReadOperation::completed(res, optr),
            ops::WR_OP => ops::WriteOperation::completed(res, optr),
            _ => {
                log::warn!("Unknown operation: {udata}");
                None
            }
        } {
            self.inner.with(|st| {
                if let Some(item) = st.streams.get_mut(id)
                    && !item.flags.contains(Flags::CLOSED)
                    && item.rd_op.pause(true)
                    && item.wr_op.pause()
                    && let Some(tx) = item.close.take()
                {
                    item.flags.insert(Flags::CLOSED);
                    let _ = tx.send(Ok(()));
                    let io = item.io.as_raw_socket() as _;
                    #[cfg(feature = "trace")]
                    log::trace!("{}: CloseWait({:?})", item.rd_op.tag(), io);
                    ntex_rt::spawn(ntex_rt::spawn_blocking(move || {
                        let _ = syscall!(SOCKET, WinSock::shutdown(io, 2));
                        let _ = syscall!(SOCKET, WinSock::closesocket(io));
                    }));
                }
            });
        }
    }

    fn cleanup(&mut self) {}
}

impl StreamOpsInner {
    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut StreamOpsStorage) -> R,
    {
        let mut storage = self.storage.take().unwrap();
        let result = f(&mut storage);
        self.storage.set(Some(storage));
        result
    }
}

impl StreamCtl {
    pub(crate) async fn shutdown(&self) -> io::Result<()> {
        let result = self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                if item.flags.contains(Flags::CLOSED) {
                    None
                } else if item.rd_op.pause(true) && item.wr_op.pause() {
                    // no outstanding ops
                    item.flags.insert(Flags::CLOSED);
                    Some(Either::Left((
                        item.rd_op.tag(),
                        item.io.as_raw_socket() as _,
                    )))
                } else {
                    let (tx, rx) = self.inner.pool.channel();
                    item.close = Some(tx);
                    Some(Either::Right(rx))
                }
            } else {
                None
            }
        });

        match result {
            Some(Either::Left((_tag, io))) => {
                #[cfg(feature = "trace")]
                log::trace!("{_tag}: Close({io:?})");
                ntex_rt::spawn_blocking(move || {
                    syscall!(SOCKET, WinSock::shutdown(io, 2)).map(|_| ())?;
                    syscall!(SOCKET, WinSock::closesocket(io)).map(|_| ())
                })
                .await
                .map_err(io::Error::other)
                .and_then(|res| res)
            }
            Some(Either::Right(rx)) => rx
                .await
                .map_err(|_| io::Error::other("Unexpected"))
                .and_then(|res| res),
            None => Ok(()),
        }
    }

    pub(crate) fn read(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                item.rd_op.read();
            }
        });
    }

    pub(crate) fn write(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                item.wr_op.write();
            }
        });
    }

    pub(crate) fn pause(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                item.rd_op.pause(false);
            }
        });
    }
}

impl StreamOpsStorage {
    fn drop_stream(&mut self, id: usize) {
        // Dropping while `StreamOps` handling event
        let item = &mut self.streams[id];
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: DropStream ({:?}) f:{:?}",
            item.rd_op.tag(),
            item.io.as_raw_socket(),
            item.flags,
        );

        if item.flags.contains(Flags::DROPPED_SEC) {
            let item = self.streams.remove(id);
            if !item.flags.contains(Flags::CLOSED) {
                let io = item.io.as_raw_socket() as _;
                ntex_rt::spawn_blocking(move || {
                    syscall!(SOCKET, WinSock::shutdown(io, 2)).map(|_| ())?;
                    syscall!(SOCKET, WinSock::closesocket(io)).map(|_| ())
                });
            }
            mem::forget(item.io);
        } else {
            item.flags.insert(Flags::DROPPED_PRI);
        }
    }

    fn drop_weak_stream(&mut self, id: usize) {
        // Dropping while `StreamOps` handling event
        let item = &mut self.streams[id];
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: DropStreamSec ({:?}) f:{:?}",
            item.rd_op.tag(),
            item.io.as_raw_socket(),
            item.flags,
        );

        if item.flags.contains(Flags::DROPPED_PRI) {
            // io is closed already, remove from storage
            let item = self.streams.remove(id);
            if !item.flags.contains(Flags::CLOSED) {
                let io = item.io.as_raw_socket() as _;
                ntex_rt::spawn_blocking(move || {
                    syscall!(SOCKET, WinSock::shutdown(io, 2)).map(|_| ())?;
                    syscall!(SOCKET, WinSock::closesocket(io)).map(|_| ())
                });
            }
            mem::forget(item.io);
        } else {
            item.flags.insert(Flags::DROPPED_SEC);
        }
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        let mut storage = self.inner.storage.take().unwrap();
        storage.drop_stream(self.id);
        self.inner.storage.set(Some(storage));
    }
}

impl WeakStreamCtl {
    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner.with(|st| f(&st.streams[self.id].io))
    }
}

impl Drop for WeakStreamCtl {
    fn drop(&mut self) {
        let mut storage = self.inner.storage.take().unwrap();
        storage.drop_weak_stream(self.id);
        self.inner.storage.set(Some(storage));
    }
}
