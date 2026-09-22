use std::{any, future::poll_fn, task::Poll};

use ntex_io::{Handle, IoContext, Readiness, types};
use ntex_rt::spawn;

use super::stream::{StreamCtl, WeakStreamCtl};

impl ntex_io::IoStream for super::TcpStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        let super::TcpStream(io, ops) = self;
        let (sctl, weak) = ops.register(io, ctx.clone());
        spawn(async move { run(sctl, ctx).await });

        Box::new(HandleWrapper(weak))
    }
}

impl ntex_io::IoStream for super::UnixStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        let super::UnixStream(io, ops) = self;
        let (sctl, weak) = ops.register(io, ctx.clone());
        spawn(async move { run(sctl, ctx).await });

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

async fn run(ctl: StreamCtl, context: ntex_io::IoContext) {
    // Handle io read readiness
    let terminate = poll_fn(|cx| {
        let mut modify = false;
        let mut readable = false;
        let mut writable = false;
        let mut terminate = false;

        let read = match context.poll_read_ready(cx) {
            Poll::Ready(Readiness::Ready) => {
                modify = true;
                readable = true;
                Poll::Pending
            }
            Poll::Ready(Readiness::Close) => Poll::Ready(()),
            Poll::Ready(Readiness::Terminate) => {
                terminate = true;
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
            Poll::Ready(Readiness::Close) => Poll::Ready(()),
            Poll::Ready(Readiness::Terminate) => {
                terminate = true;
                Poll::Ready(())
            }
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
        } else {
            Poll::Ready(terminate)
        }
    })
    .await;

    log::trace!("{}: Shuting down io", context.tag());
    // Both directions can report `Close` in the same poll, which leaves the
    // last armed interest in place. Drop it before teardown, so the reactor
    // cannot deliver an event for a socket that is being closed.
    ctl.interest(false, false);

    let result = if terminate {
        // The connection was force-closed, so buffered output is discarded on
        // purpose. The socket is aborted rather than drained and shut down, so
        // that the peer sees an RST and cannot mistake a truncated stream for a
        // complete one. Dropping the control handle detaches the socket from
        // the reactor and closes the descriptor.
        ctl.abort();
        drop(ctl);
        Ok(())
    } else {
        ctl.shutdown().await
    };
    log::trace!("{}: Shutdown complete {result:?}", context.tag());
    context.stopped(result.err());
}
