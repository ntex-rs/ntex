use std::{any, future::poll_fn, task::Poll};

use compio_net::TcpStream;
use ntex_io::{
    types, Handle, IoStream, ReadContext, ReadStatus, WriteContext, WriteStatus,
};

use crate::driver::{CompioOps, StreamCtl};

impl IoStream for crate::TcpStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0.clone();
        let ctl = CompioOps::current().register(io, read.clone(), write.clone());
        compio_runtime::spawn(async move { run(ctl, read, write).await }).detach();

        Some(Box::new(HandleWrapper(self.0)))
    }
}

struct HandleWrapper(TcpStream);

impl Handle for HandleWrapper {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            if let Ok(addr) = self.0.peer_addr() {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
    }
}

async fn run(ctl: StreamCtl, read: ReadContext, write: WriteContext) {
    // Handle io read readiness
    poll_fn(|cx| {
        ctl.register(cx.waker());

        let read = match read.poll_ready(cx) {
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

        let write = match write.poll_ready(cx) {
            Poll::Ready(WriteStatus::Ready) => {
                ctl.resume_write();
                Poll::Pending
            }
            Poll::Ready(WriteStatus::Shutdown) => Poll::Ready(()),
            Poll::Ready(WriteStatus::Terminate) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        };

        if read.is_pending() && write.is_pending() {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
}
