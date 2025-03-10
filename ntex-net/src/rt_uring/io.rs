use std::{any, future::poll_fn, io, task::Poll};

use ntex_io::{
    types, Handle, IoContext, IoStream, ReadContext, ReadStatus, WriteContext, WriteStatus,
};
use ntex_neon::{net::TcpStream, net::UnixStream, spawn};

use super::driver::{Closable, StreamOps, StreamCtl};

impl IoStream for super::TcpStream {
    fn start(self, read: ReadContext, _: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let context = read.context();
        let ctl = StreamOps::current().register(io, context.clone());
        let ctl2 = ctl.clone();
        spawn(async move { run(ctl, context).await }).detach();

        Some(Box::new(HandleWrapper(ctl2)))
    }
}

impl IoStream for super::UnixStream {
    fn start(self, read: ReadContext, _: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let context = read.context();
        let ctl = StreamOps::current().register(io, context.clone());
        spawn(async move { run(ctl, context).await }).detach();

        None
    }
}

struct HandleWrapper(StreamCtl<TcpStream>);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            let addr = self.0.with_io(|io| io.and_then(|io| io.peer_addr().ok()));
            if let Some(addr) = addr {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
    }
}

impl Closable for TcpStream {
    async fn close(self) -> io::Result<()> {
        TcpStream::close(self).await
    }
}

impl Closable for UnixStream {
    async fn close(self) -> io::Result<()> {
        UnixStream::close(self).await
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Status {
    Shutdown,
    Terminate,
}

async fn run<T: Closable>(ctl: StreamCtl<T>, context: IoContext) {
    // Handle io read readiness
    let st = poll_fn(|cx| {
        let read = match context.poll_read_ready(cx) {
            Poll::Ready(ReadStatus::Ready) => {
                ctl.resume_read();
                Poll::Pending
            }
            Poll::Ready(ReadStatus::Terminate) => Poll::Ready(()),
            Poll::Pending => {
                ctl.pause_read();
                Poll::Pending
            }
        };

        let write = match context.poll_write_ready(cx) {
            Poll::Ready(WriteStatus::Ready) => {
                ctl.resume_write();
                Poll::Pending
            }
            Poll::Ready(WriteStatus::Shutdown) => Poll::Ready(Status::Shutdown),
            Poll::Ready(WriteStatus::Terminate) => Poll::Ready(Status::Terminate),
            Poll::Pending => Poll::Pending,
        };

        if read.is_pending() && write.is_pending() {
            Poll::Pending
        } else if write.is_ready() {
            write
        } else {
            Poll::Ready(Status::Terminate)
        }
    })
    .await;

    ctl.resume_write();
    context.shutdown(st == Status::Shutdown).await;

    ctl.pause_all();
    let result = ctl.close().await;

    context.stopped(result.err());
}
