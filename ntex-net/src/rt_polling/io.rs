use std::{any, future::poll_fn, task::Poll};

use ntex_io::{types, Handle, ReadContext, Readiness, WriteContext};
use ntex_rt::spawn;

use super::driver::{StreamCtl, StreamOps};

impl ntex_io::IoStream for super::TcpStream {
    fn start(self, read: ReadContext, _: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let context = read.context();
        let ctl = StreamOps::current().register(io, context.clone());
        let ctl2 = ctl.clone();
        spawn(async move { run(ctl, context).await });

        Some(Box::new(HandleWrapper(ctl2)))
    }
}

impl ntex_io::IoStream for super::UnixStream {
    fn start(self, read: ReadContext, _: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let context = read.context();
        let ctl = StreamOps::current().register(io, context.clone());
        spawn(async move { run(ctl, context).await });

        None
    }
}

struct HandleWrapper(StreamCtl);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            let addr = self.0.with(|io| io.peer_addr().ok());
            if let Some(addr) = addr.and_then(|addr| addr.as_socket()) {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
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
            Poll::Ready(Readiness::Ready) => {
                modify = true;
                readable = true;
                Poll::Pending
            }
            Poll::Ready(Readiness::Shutdown) | Poll::Ready(Readiness::Terminate) => {
                Poll::Ready(())
            }
            Poll::Pending => {
                modify = true;
                Poll::Pending
            }
        };

        let write = match context.poll_write_ready(cx) {
            Poll::Ready(Readiness::Ready) => {
                modify = true;
                writable = true;
                Poll::Pending
            }
            Poll::Ready(Readiness::Shutdown) => Poll::Ready(Status::Shutdown),
            Poll::Ready(Readiness::Terminate) => Poll::Ready(Status::Terminate),
            Poll::Pending => {
                modify = true;
                Poll::Pending
            }
        };

        if modify {
            ctl.interest(readable, writable);
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

    log::trace!("{}: Shuting down io", context.tag());
    if !context.is_stopped() {
        let flush = st == Status::Shutdown;
        poll_fn(|cx| {
            ctl.interest(true, true);
            context.shutdown(flush, cx)
        })
        .await;
    }

    let result = ctl.shutdown().await;
    log::trace!("{}: Shutdown complete {result:?}", context.tag());
    if !context.is_stopped() {
        context.stop(result.err());
    }
}
