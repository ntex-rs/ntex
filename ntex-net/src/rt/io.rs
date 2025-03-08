use std::{any, future::poll_fn, io, task::Poll};

use ntex_io::{
    types, Handle, IoStream, ReadContext, ReadStatus, WriteContext, WriteStatus,
};
use ntex_runtime::{net::TcpStream, net::UnixStream, spawn};

use super::driver::{CompioOps, StreamCtl};

impl IoStream for super::TcpStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let ctl = CompioOps::current().register(io, read.clone(), write.clone());
        let ctl2 = ctl.clone();
        spawn(async move { run(ctl, read, write).await }).detach();

        Some(Box::new(HandleWrapper(ctl2)))
    }
}

impl IoStream for super::UnixStream {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let io = self.0;
        let ctl = CompioOps::current().register(io, read.clone(), write.clone());
        spawn(async move { run(ctl, read, write).await }).detach();

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

trait Closable {
    async fn close(self) -> io::Result<()>;
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

async fn run<T: Closable>(ctl: StreamCtl<T>, read: ReadContext, write: WriteContext) {
    // Handle io read readiness
    let st = poll_fn(|cx| {
        read.shutdown_filters(cx);

        let read_st = read.poll_ready(cx);
        let write_st = write.poll_ready(cx);
        //println!("\n\n");
        //println!(
        //    "IO2 read-st {:?}, write-st: {:?}, flags: {:?}",
        //    read_st,
        //    write_st,
        //    read.io().flags()
        //);
        //println!("\n\n");

        //let read = match read.poll_ready(cx) {
        let read = match read_st {
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

        let write = match write_st {
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
    if st == Status::Shutdown {
        write.wait_for_shutdown(true).await;
    } else {
        write.wait_for_shutdown(false).await;
    }

    ctl.pause_all();
    let io = ctl.take_io().unwrap();
    let result = io.close().await;

    read.set_stopped(result.err());
}
