//! utilities and helpers for testing
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll, Waker};
use std::{any, cell::RefCell, cmp, fmt, future::Future, io, mem, net, pin::Pin, rc::Rc};

use ntex_bytes::{Buf, BufMut, Bytes, BytesVec};
use ntex_util::future::poll_fn;
use ntex_util::time::{sleep, Millis, Sleep};

use crate::{types, Handle, IoStream, ReadContext, ReadStatus, WriteContext, WriteStatus};

#[derive(Default)]
struct AtomicWaker(Arc<Mutex<RefCell<Option<Waker>>>>);

impl AtomicWaker {
    fn wake(&self) {
        if let Some(waker) = self.0.lock().unwrap().borrow_mut().take() {
            waker.wake()
        }
    }
}

impl fmt::Debug for AtomicWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AtomicWaker")
    }
}

/// Async io stream
#[derive(Debug)]
pub struct IoTest {
    tp: Type,
    peer_addr: Option<net::SocketAddr>,
    state: Arc<Mutex<RefCell<State>>>,
    local: Arc<Mutex<RefCell<Channel>>>,
    remote: Arc<Mutex<RefCell<Channel>>>,
}

bitflags::bitflags! {
    struct IoTestFlags: u8 {
        const FLUSHED = 0b0000_0001;
        const CLOSED  = 0b0000_0010;
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Type {
    Client,
    Server,
    ClientClone,
    ServerClone,
}

#[derive(Copy, Clone, Default, Debug)]
struct State {
    client_dropped: bool,
    server_dropped: bool,
}

#[derive(Default, Debug)]
struct Channel {
    buf: BytesVec,
    buf_cap: usize,
    flags: IoTestFlags,
    waker: AtomicWaker,
    read: IoTestState,
    write: IoTestState,
}

unsafe impl Sync for Channel {}
unsafe impl Send for Channel {}

impl Channel {
    fn is_closed(&self) -> bool {
        self.flags.contains(IoTestFlags::CLOSED)
    }
}

impl Default for IoTestFlags {
    fn default() -> Self {
        IoTestFlags::empty()
    }
}

#[derive(Debug, Default)]
enum IoTestState {
    #[default]
    Ok,
    Pending,
    Close,
    Err(io::Error),
}

impl IoTest {
    /// Create a two interconnected streams
    pub fn create() -> (IoTest, IoTest) {
        let local = Arc::new(Mutex::new(RefCell::new(Channel::default())));
        let remote = Arc::new(Mutex::new(RefCell::new(Channel::default())));
        let state = Arc::new(Mutex::new(RefCell::new(State::default())));

        (
            IoTest {
                tp: Type::Client,
                peer_addr: None,
                local: local.clone(),
                remote: remote.clone(),
                state: state.clone(),
            },
            IoTest {
                state,
                peer_addr: None,
                tp: Type::Server,
                local: remote,
                remote: local,
            },
        )
    }

    pub fn is_client_dropped(&self) -> bool {
        self.state.lock().unwrap().borrow().client_dropped
    }

    pub fn is_server_dropped(&self) -> bool {
        self.state.lock().unwrap().borrow().server_dropped
    }

    /// Check if channel is closed from remoote side
    pub fn is_closed(&self) -> bool {
        self.remote.lock().unwrap().borrow().is_closed()
    }

    /// Set peer addr
    pub fn set_peer_addr(mut self, addr: net::SocketAddr) -> Self {
        self.peer_addr = Some(addr);
        self
    }

    /// Set read to Pending state
    pub fn read_pending(&self) {
        self.remote.lock().unwrap().borrow_mut().read = IoTestState::Pending;
    }

    /// Set read to error
    pub fn read_error(&self, err: io::Error) {
        let channel = self.remote.lock().unwrap();
        channel.borrow_mut().read = IoTestState::Err(err);
        channel.borrow().waker.wake();
    }

    /// Set write error on remote side
    pub fn write_error(&self, err: io::Error) {
        self.local.lock().unwrap().borrow_mut().write = IoTestState::Err(err);
        self.remote.lock().unwrap().borrow().waker.wake();
    }

    /// Access read buffer.
    pub fn local_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        let guard = self.local.lock().unwrap();
        let mut ch = guard.borrow_mut();
        f(&mut ch.buf)
    }

    /// Access remote buffer.
    pub fn remote_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        let guard = self.remote.lock().unwrap();
        let mut ch = guard.borrow_mut();
        f(&mut ch.buf)
    }

    /// Closed remote side.
    pub async fn close(&self) {
        {
            let guard = self.remote.lock().unwrap();
            let mut remote = guard.borrow_mut();
            remote.read = IoTestState::Close;
            remote.waker.wake();
            log::trace!("close remote socket");
        }
        sleep(Millis(35)).await;
    }

    /// Add extra data to the remote buffer and notify reader
    pub fn write<T: AsRef<[u8]>>(&self, data: T) {
        let guard = self.remote.lock().unwrap();
        let mut write = guard.borrow_mut();
        write.buf.extend_from_slice(data.as_ref());
        write.waker.wake();
    }

    /// Read any available data
    pub fn remote_buffer_cap(&self, cap: usize) {
        // change cap
        self.local.lock().unwrap().borrow_mut().buf_cap = cap;
        // wake remote
        self.remote.lock().unwrap().borrow().waker.wake();
    }

    /// Read any available data
    pub fn read_any(&self) -> Bytes {
        self.local.lock().unwrap().borrow_mut().buf.split().freeze()
    }

    /// Read data, if data is not available wait for it
    pub async fn read(&self) -> Result<Bytes, io::Error> {
        if self.local.lock().unwrap().borrow().buf.is_empty() {
            poll_fn(|cx| {
                let guard = self.local.lock().unwrap();
                let read = guard.borrow_mut();
                if read.buf.is_empty() {
                    let closed = match self.tp {
                        Type::Client | Type::ClientClone => {
                            self.is_server_dropped() || read.is_closed()
                        }
                        Type::Server | Type::ServerClone => self.is_client_dropped(),
                    };
                    if closed {
                        Poll::Ready(())
                    } else {
                        *read.waker.0.lock().unwrap().borrow_mut() =
                            Some(cx.waker().clone());
                        drop(read);
                        drop(guard);
                        Poll::Pending
                    }
                } else {
                    Poll::Ready(())
                }
            })
            .await;
        }
        Ok(self.local.lock().unwrap().borrow_mut().buf.split().freeze())
    }

    pub fn poll_read_buf(
        &self,
        cx: &mut Context<'_>,
        buf: &mut BytesVec,
    ) -> Poll<io::Result<usize>> {
        let guard = self.local.lock().unwrap();
        let mut ch = guard.borrow_mut();
        *ch.waker.0.lock().unwrap().borrow_mut() = Some(cx.waker().clone());

        if !ch.buf.is_empty() {
            let size = std::cmp::min(ch.buf.len(), buf.remaining_mut());
            let b = ch.buf.split_to(size);
            buf.put_slice(&b);
            return Poll::Ready(Ok(size));
        }

        match mem::take(&mut ch.read) {
            IoTestState::Ok => Poll::Pending,
            IoTestState::Close => {
                ch.read = IoTestState::Close;
                Poll::Ready(Ok(0))
            }
            IoTestState::Pending => Poll::Pending,
            IoTestState::Err(e) => Poll::Ready(Err(e)),
        }
    }

    pub fn poll_write_buf(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let guard = self.remote.lock().unwrap();
        let mut ch = guard.borrow_mut();

        match mem::take(&mut ch.write) {
            IoTestState::Ok => {
                let cap = cmp::min(buf.len(), ch.buf_cap);
                if cap > 0 {
                    ch.buf.extend(&buf[..cap]);
                    ch.buf_cap -= cap;
                    ch.flags.remove(IoTestFlags::FLUSHED);
                    ch.waker.wake();
                    Poll::Ready(Ok(cap))
                } else {
                    *self
                        .local
                        .lock()
                        .unwrap()
                        .borrow_mut()
                        .waker
                        .0
                        .lock()
                        .unwrap()
                        .borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
            IoTestState::Close => Poll::Ready(Ok(0)),
            IoTestState::Pending => {
                *self
                    .local
                    .lock()
                    .unwrap()
                    .borrow_mut()
                    .waker
                    .0
                    .lock()
                    .unwrap()
                    .borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
            IoTestState::Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl Clone for IoTest {
    fn clone(&self) -> Self {
        let tp = match self.tp {
            Type::Server => Type::ServerClone,
            Type::Client => Type::ClientClone,
            val => val,
        };

        IoTest {
            tp,
            local: self.local.clone(),
            remote: self.remote.clone(),
            state: self.state.clone(),
            peer_addr: self.peer_addr,
        }
    }
}

impl Drop for IoTest {
    fn drop(&mut self) {
        let mut state = *self.state.lock().unwrap().borrow();
        match self.tp {
            Type::Server => state.server_dropped = true,
            Type::Client => state.client_dropped = true,
            _ => (),
        }
        *self.state.lock().unwrap().borrow_mut() = state;

        let guard = self.remote.lock().unwrap();
        let mut remote = guard.borrow_mut();
        remote.read = IoTestState::Close;
        remote.waker.wake();
        log::trace!("drop remote socket");
    }
}

impl IoStream for IoTest {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let io = Rc::new(self);

        ntex_util::spawn(ReadTask {
            io: io.clone(),
            state: read,
        });
        ntex_util::spawn(WriteTask {
            io: io.clone(),
            state: write,
            st: IoWriteState::Processing(None),
        });

        Some(Box::new(io))
    }
}

impl Handle for Rc<IoTest> {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::PeerAddr>() {
            if let Some(addr) = self.peer_addr {
                return Some(Box::new(types::PeerAddr(addr)));
            }
        }
        None
    }
}

/// Read io task
struct ReadTask {
    io: Rc<IoTest>,
    state: ReadContext,
}

impl Future for ReadTask {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_ref();

        this.state.with_buf(|buf, hw, lw| {
            match this.state.poll_ready(cx) {
                Poll::Ready(ReadStatus::Terminate) => {
                    log::trace!("read task is instructed to terminate");
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(ReadStatus::Ready) => {
                    let io = &this.io;

                    // read data from socket
                    let mut new_bytes = 0;
                    loop {
                        // make sure we've got room
                        let remaining = buf.remaining_mut();
                        if remaining < lw {
                            buf.reserve(hw - remaining);
                        }
                        match io.poll_read_buf(cx, buf) {
                            Poll::Pending => {
                                log::trace!(
                                    "no more data in io stream, read: {:?}",
                                    new_bytes
                                );
                                break;
                            }
                            Poll::Ready(Ok(n)) => {
                                if n == 0 {
                                    log::trace!("io stream is disconnected");
                                    return Poll::Ready(Ok(()));
                                } else {
                                    new_bytes += n;
                                    if buf.len() >= hw {
                                        log::trace!(
                                            "high water mark pause reading, read: {:?}",
                                            new_bytes
                                        );
                                        break;
                                    }
                                }
                            }
                            Poll::Ready(Err(err)) => {
                                log::trace!("read task failed on io {:?}", err);
                                return Poll::Ready(Err(err));
                            }
                        }
                    }

                    Poll::Pending
                }
                Poll::Pending => Poll::Pending,
            }
        })
    }
}

#[derive(Debug)]
enum IoWriteState {
    Processing(Option<Sleep>),
    Shutdown(Option<Sleep>, Shutdown),
}

#[derive(Debug)]
enum Shutdown {
    None,
    Flushed,
    Stopping,
}

/// Write io task
struct WriteTask {
    st: IoWriteState,
    io: Rc<IoTest>,
    state: WriteContext,
}

impl Future for WriteTask {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();

        match this.st {
            IoWriteState::Processing(ref mut delay) => {
                match this.state.poll_ready(cx) {
                    Poll::Ready(WriteStatus::Ready) => {
                        // flush framed instance
                        match ready!(flush_io(&this.io, &this.state, cx)) {
                            Ok(()) => Poll::Pending,
                            Err(e) => {
                                this.state.close(Some(e));
                                Poll::Ready(())
                            }
                        }
                    }
                    Poll::Ready(WriteStatus::Timeout(time)) => {
                        if delay.is_none() {
                            *delay = Some(sleep(time));
                        }
                        self.poll(cx)
                    }
                    Poll::Ready(WriteStatus::Shutdown(time)) => {
                        log::trace!("write task is instructed to shutdown");

                        let timeout = if let Some(delay) = delay.take() {
                            delay
                        } else {
                            sleep(time)
                        };

                        this.st = IoWriteState::Shutdown(Some(timeout), Shutdown::None);
                        self.poll(cx)
                    }
                    Poll::Ready(WriteStatus::Terminate) => {
                        log::trace!("write task is instructed to terminate");
                        // shutdown WRITE side
                        this.io
                            .local
                            .lock()
                            .unwrap()
                            .borrow_mut()
                            .flags
                            .insert(IoTestFlags::CLOSED);
                        this.state.close(None);
                        Poll::Ready(())
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            IoWriteState::Shutdown(ref mut delay, ref mut st) => {
                // close WRITE side and wait for disconnect on read side.
                // use disconnect timeout, otherwise it could hang forever.
                loop {
                    match st {
                        Shutdown::None => {
                            // flush write buffer
                            match flush_io(&this.io, &this.state, cx) {
                                Poll::Ready(Ok(())) => {
                                    *st = Shutdown::Flushed;
                                    continue;
                                }
                                Poll::Ready(Err(err)) => {
                                    log::trace!(
                                        "write task is closed with err during flush {:?}",
                                        err
                                    );
                                    this.state.close(Some(err));
                                    return Poll::Ready(());
                                }
                                Poll::Pending => (),
                            }
                        }
                        Shutdown::Flushed => {
                            // shutdown WRITE side
                            this.io
                                .local
                                .lock()
                                .unwrap()
                                .borrow_mut()
                                .flags
                                .insert(IoTestFlags::CLOSED);
                            *st = Shutdown::Stopping;
                            continue;
                        }
                        Shutdown::Stopping => {
                            // read until 0 or err
                            let io = &this.io;
                            loop {
                                let mut buf = BytesVec::new();
                                match io.poll_read_buf(cx, &mut buf) {
                                    Poll::Ready(Err(e)) => {
                                        this.state.close(Some(e));
                                        log::trace!("write task is stopped");
                                        return Poll::Ready(());
                                    }
                                    Poll::Ready(Ok(0)) => {
                                        this.state.close(None);
                                        log::trace!("write task is stopped");
                                        return Poll::Ready(());
                                    }
                                    Poll::Pending => break,
                                    _ => (),
                                }
                            }
                        }
                    }

                    // disconnect timeout
                    if let Some(ref delay) = delay {
                        if delay.poll_elapsed(cx).is_pending() {
                            return Poll::Pending;
                        }
                    }
                    log::trace!("write task is stopped after delay");
                    this.state.close(None);
                    return Poll::Ready(());
                }
            }
        }
    }
}

/// Flush write buffer to underlying I/O stream.
pub(super) fn flush_io(
    io: &IoTest,
    state: &WriteContext,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    state.with_buf(|buf| {
        if let Some(buf) = buf {
            let len = buf.len();

            if len != 0 {
                log::trace!("flushing framed transport: {}", len);

                let mut written = 0;
                let result = loop {
                    break match io.poll_write_buf(cx, &buf[written..]) {
                        Poll::Ready(Ok(n)) => {
                            if n == 0 {
                                log::trace!(
                                    "disconnected during flush, written {}",
                                    written
                                );
                                Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::WriteZero,
                                    "failed to write frame to transport",
                                )))
                            } else {
                                written += n;
                                if written == len {
                                    buf.clear();
                                    Poll::Ready(Ok(()))
                                } else {
                                    continue;
                                }
                            }
                        }
                        Poll::Pending => {
                            // remove written data
                            buf.advance(written);
                            Poll::Pending
                        }
                        Poll::Ready(Err(e)) => {
                            log::trace!("error during flush: {}", e);
                            Poll::Ready(Err(e))
                        }
                    };
                };
                log::trace!("flushed {} bytes", written);
                return result;
            }
        }
        Poll::Ready(Ok(()))
    })
}

#[cfg(test)]
#[allow(clippy::redundant_clone)]
mod tests {
    use super::*;
    use ntex_util::future::lazy;

    #[ntex::test]
    async fn basic() {
        let (client, server) = IoTest::create();
        assert_eq!(client.tp, Type::Client);
        assert_eq!(client.clone().tp, Type::ClientClone);
        assert_eq!(server.tp, Type::Server);
        assert_eq!(server.clone().tp, Type::ServerClone);
        assert!(format!("{:?}", server).contains("IoTest"));
        assert!(format!("{:?}", AtomicWaker::default()).contains("AtomicWaker"));

        server.read_pending();
        let mut buf = BytesVec::new();
        let res = lazy(|cx| client.poll_read_buf(cx, &mut buf)).await;
        assert!(res.is_pending());

        server.read_pending();
        let res = lazy(|cx| server.poll_write_buf(cx, b"123")).await;
        assert!(res.is_pending());

        assert!(!server.is_client_dropped());
        drop(client);
        assert!(server.is_client_dropped());

        let server2 = server.clone();
        assert!(!server2.is_server_dropped());
        drop(server);
        assert!(server2.is_server_dropped());

        let res = lazy(|cx| server2.poll_write_buf(cx, b"123")).await;
        assert!(res.is_pending());
    }
}
