use std::{any, future::poll_fn, task::Context, task::Poll};

use ntex_io::{Handle, IoContext, Readiness, types};
use ntex_rt::spawn;

use super::stream::{StreamCtl, WeakStreamCtl};

impl ntex_io::IoStream for super::TcpStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        let Self(io, ops) = self;
        let (ctl, ctl2) = ops.register(io, ctx.clone(), true);
        spawn(async move { run(ctl, ctx).await });

        Box::new(HandleWrapper(ctl2))
    }
}

impl ntex_io::IoStream for super::UnixStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        let Self(io, ops) = self;
        let (ctl, ctl2) = ops.register(io, ctx.clone(), false);
        spawn(async move { run(ctl, ctx).await });

        Box::new(HandleWrapper(ctl2))
    }
}

struct HandleWrapper(WeakStreamCtl);

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
    let st = poll_fn(|cx| poll_readiness(&ctl, &ctx, cx)).await;

    if !ctx.is_stopped() {
        let flush = st == Status::Shutdown;
        poll_fn(|cx| {
            let _ = poll_readiness(&ctl, &ctx, cx);
            ctx.shutdown(flush, cx)
        })
        .await;
    }

    let result = ctl.shutdown().await;
    if !ctx.is_stopped() {
        ctx.stop(result.err());
    }
}

/// Handle ctx readiness
fn poll_readiness(ctl: &StreamCtl, ctx: &IoContext, cx: &mut Context<'_>) -> Poll<Status> {
    let read = match ctx.poll_read_ready(cx) {
        Poll::Ready(Readiness::Ready) => {
            ctl.resume_read();
            Poll::Pending
        }
        Poll::Ready(Readiness::Shutdown | Readiness::Terminate) => Poll::Ready(()),
        Poll::Pending => {
            ctl.pause_read();
            Poll::Pending
        }
    };

    let write = match ctx.poll_write_ready(cx) {
        Poll::Ready(Readiness::Ready) => {
            ctl.resume_write();
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
}
