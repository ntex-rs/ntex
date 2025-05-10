use std::{any, future::poll_fn, task::Poll};

use ntex_io::{types, Handle, IoContext, ReadContext, Readiness};
use ntex_rt::spawn;

use super::driver::{StreamCtl, StreamOps};

impl ntex_io::IoStream for super::TcpStream {
    fn start(self, read: ReadContext, _: ntex_io::WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let context = read.context();
        let ctl = StreamOps::current().register(io, context.clone());
        let ctl2 = ctl.clone();
        spawn(async move { run(ctl, context).await });

        Some(Box::new(HandleWrapper(ctl2)))
    }
}

impl ntex_io::IoStream for super::UnixStream {
    fn start(self, read: ReadContext, _: ntex_io::WriteContext) -> Option<Box<dyn Handle>> {
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
            let addr = self.0.with_io(|io| io.peer_addr().ok());
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

async fn run(ctl: StreamCtl, ctx: IoContext) {
    // Handle io readiness
    let st = poll_fn(|cx| {
        let read = match ctx.poll_read_ready(cx) {
            Poll::Ready(Readiness::Ready) => {
                ctl.resume_read(&ctx);
                Poll::Pending
            }
            Poll::Ready(Readiness::Shutdown) | Poll::Ready(Readiness::Terminate) => {
                Poll::Ready(())
            }
            Poll::Pending => {
                ctl.pause_read();
                Poll::Pending
            }
        };

        let write = match ctx.poll_write_ready(cx) {
            Poll::Ready(Readiness::Ready) => {
                ctl.resume_write(&ctx);
                Poll::Pending
            }
            Poll::Ready(Readiness::Shutdown) => Poll::Ready(Status::Shutdown),
            Poll::Ready(Readiness::Terminate) => Poll::Ready(Status::Terminate),
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

    if !ctx.is_stopped() {
        let flush = st == Status::Shutdown;
        poll_fn(|cx| {
            ctl.resume_read(&ctx);
            ctl.resume_write(&ctx);
            ctx.shutdown(flush, cx)
        })
        .await;
    }

    let result = ctl.shutdown().await;
    if !ctx.is_stopped() {
        ctx.stop(result.err());
    }
}
