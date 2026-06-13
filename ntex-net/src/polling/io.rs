use std::{any, future::poll_fn, task::Poll};

use ntex_io::{Handle, IoContext, Readiness, types};
use ntex_rt::spawn;

use super::stream::{StreamCtl, WeakStreamCtl};

impl ntex_io::IoStream for super::TcpStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        let super::TcpStream(io, ops) = self;
        let (ctl, weak) = ops.register(io, ctx.clone());
        spawn(async move { run(ctl, ctx).await });

        Box::new(HandleWrapper(weak))
    }
}

impl ntex_io::IoStream for super::UnixStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        let super::UnixStream(io, ops) = self;
        let (ctl, weak) = ops.register(io, ctx.clone());
        spawn(async move { run(ctl, ctx).await });

        Box::new(HandleWrapper(weak))
    }
}

struct HandleWrapper(WeakStreamCtl);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            let addr = self.0.with_socket(|io| io.peer_addr().ok());
            if let Some(addr) = addr.and_then(|addr| addr.as_socket()) {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
    }

    fn write(&self, _: &IoContext) {
        self.0.write();
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Status {
    Shutdown,
    Terminate,
}

async fn run(ctl: StreamCtl, ctx: ntex_io::IoContext) {
    // Handle io read readiness
    let st = poll_fn(|cx| {
        let mut modify = false;
        let mut readable = false;
        let mut writable = false;

        let read = match ctx.poll_read_ready(cx) {
            Poll::Ready(Readiness::Ready) => {
                modify = true;
                readable = true;
                Poll::Pending
            }
            Poll::Ready(Readiness::Shutdown | Readiness::Terminate) => Poll::Ready(()),
            Poll::Pending => {
                modify = true;
                Poll::Pending
            }
        };

        let write = match ctx.poll_write_ready(cx) {
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

    log::trace!("{}: Shuting down io", ctx.tag());
    if !ctx.is_stopped() {
        let flush = st == Status::Shutdown;
        poll_fn(|cx| {
            ctl.interest(true, true);
            ctx.shutdown(flush, cx)
        })
        .await;
    }

    let result = ctl.shutdown().await;
    log::trace!("{}: Shutdown complete {result:?}", ctx.tag());
    if !ctx.is_stopped() {
        ctx.stop(result.err());
    }
}
