use std::{any, future::poll_fn, mem, os::fd::AsRawFd, task::Poll};

use ntex_io::{
    types, Handle, IoContext, IoStream, ReadContext, ReadStatus, WriteContext, WriteStatus,
};
use ntex_rt::spawn;
use socket2::Socket;

use super::driver::{StreamCtl, StreamOps};

impl IoStream for super::TcpStream {
    fn start(self, read: ReadContext, _: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let context = read.context();
        let ctl = StreamOps::current().register(io.as_raw_fd(), context.clone());
        spawn(async move { run(ctl, context).await });

        Some(Box::new(HandleWrapper(Some(io))))
    }
}

impl IoStream for super::UnixStream {
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
            let addr = self.0.as_ref().unwrap().peer_addr().ok();
            if let Some(addr) = addr.and_then(|addr| addr.as_socket()) {
                return Some(Box::new(types::PeerAddr(addr)));
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

async fn run(ctl: StreamCtl, context: IoContext) {
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
            if !ctl.modify(readable, writable) {
                return Poll::Ready(Status::Terminate);
            }
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

    if st != Status::Terminate {
        if ctl.modify(false, true) {
            context.shutdown(st == Status::Shutdown).await;
        }
    }
    context.stopped(ctl.close().await.err());
}
