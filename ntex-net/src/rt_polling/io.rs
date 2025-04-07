use std::{any, future::poll_fn, mem, os::fd::AsRawFd, task::Poll};

use ntex_io::{types, Handle, ReadContext, ReadStatus, WriteContext, WriteStatus};
use ntex_rt::spawn;
use socket2::Socket;

use super::driver::{StreamCtl, StreamOps};

impl ntex_io::IoStream for super::TcpStream {
    fn start(self, read: ReadContext, _: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let context = read.context();
        let ctl = StreamOps::current().register(io.as_raw_fd(), context.clone());
        spawn(async move { run(ctl, context).await });

        Some(Box::new(HandleWrapper(Some(io))))
    }
}

impl ntex_io::IoStream for super::UnixStream {
    fn start(self, read: ReadContext, _: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let context = read.context();
        let ctl = StreamOps::current().register(io.as_raw_fd(), context.clone());
        spawn(async move { run(ctl, context).await });
        mem::forget(io);

        None
    }
}

struct HandleWrapper(Option<Socket>);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            match self.0.as_ref().unwrap().peer_addr() {
                Ok(addr) => {
                    if let Some(addr) = addr.as_socket() {
                        return Some(Box::new(types::PeerAddr(addr)));
                    }
                }
                Err(err) => {
                    log::error!("Cannot get peer addr: {:?}", err);
                }
            }
        }
        None
    }
}

impl Drop for HandleWrapper {
    fn drop(&mut self) {
        mem::forget(self.0.take());
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Status {
    Shutdown,
    Terminate,
}

async fn run(ctl: StreamCtl, context: ntex_io::IoContext) {
    // Handle io read readiness
    let st = poll_fn(|cx| {
        let mut modify = false;
        let mut readable = false;
        let mut writable = false;

        let read = match context.poll_read_ready(cx) {
            Poll::Ready(ReadStatus::Ready) => {
                modify = true;
                readable = true;
                Poll::Pending
            }
            Poll::Ready(ReadStatus::Terminate) => Poll::Ready(()),
            Poll::Pending => {
                modify = true;
                Poll::Pending
            }
        };

        let write = match context.poll_write_ready(cx) {
            Poll::Ready(WriteStatus::Ready) => {
                modify = true;
                writable = true;
                Poll::Pending
            }
            Poll::Ready(WriteStatus::Shutdown) => Poll::Ready(Status::Shutdown),
            Poll::Ready(WriteStatus::Terminate) => Poll::Ready(Status::Terminate),
            Poll::Pending => Poll::Pending,
        };

        if modify {
            ctl.modify(readable, writable);
        }

        if read.is_pending() && write.is_pending() {
            Poll::Pending
        } else if write.is_ready() {
            write
        } else {
            Poll::Ready(Status::Terminate)
        }
    })
    .await;

    // write buf
    if st != Status::Terminate {
        ctl.modify(false, true);
        context.shutdown(st == Status::Shutdown).await;
    }

    let res = ctl.close().await;
    if !context.is_stopped() {
        context.stopped(res.err());
    }
}
