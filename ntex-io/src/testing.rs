use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::{any, cmp, fmt, future::Future, io, mem, net, pin::Pin, rc::Rc};

use ntex_bytes::{Buf, BufMut, BytesMut};
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
    state: Arc<Cell<State>>,
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
    buf: BytesMut,
    buf_cap: usize,
    flags: IoTestFlags,
    waker: AtomicWaker,
    read: IoTestState,
    write: IoTestState,
}

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

#[derive(Debug)]
enum IoTestState {
    Ok,
    Pending,
    Close,
    Err(io::Error),
}

impl Default for IoTestState {
    fn default() -> Self {
        IoTestState::Ok
    }
}

impl IoTest {
    /// Create a two interconnected streams
    pub fn create() -> (IoTest, IoTest) {
        let local = Arc::new(Mutex::new(RefCell::new(Channel::default())));
        let remote = Arc::new(Mutex::new(RefCell::new(Channel::default())));
        let state = Arc::new(Cell::new(State::default()));

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
        self.state.get().client_dropped
    }

    pub fn is_server_dropped(&self) -> bool {
        self.state.get().server_dropped
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
        F: FnOnce(&mut BytesMut) -> R,
    {
        let guard = self.local.lock().unwrap();
        let mut ch = guard.borrow_mut();
        f(&mut ch.buf)
    }

    /// Access remote buffer.
    pub fn remote_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
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
    pub fn read_any(&self) -> BytesMut {
        self.local.lock().unwrap().borrow_mut().buf.split()
    }

    /// Read data, if data is not available wait for it
    pub async fn read(&self) -> Result<BytesMut, io::Error> {
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
        Ok(self.local.lock().unwrap().borrow_mut().buf.split())
    }

    pub fn poll_read_buf(
        &self,
        cx: &mut Context<'_>,
        buf: &mut BytesMut,
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
        let mut state = self.state.get();
        match self.tp {
            Type::Server => state.server_dropped = true,
            Type::Client => state.client_dropped = true,
            _ => (),
        }
        self.state.set(state);
    }
}

impl IoStream for IoTest {
    fn start(self, read: ReadContext, write: WriteContext) -> Option<Box<dyn Handle>> {
        let io = Rc::new(self);

        crate::rt::spawn(ReadTask {
            io: io.clone(),
            state: read,
        });
        crate::rt::spawn(WriteTask {
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

        match this.state.poll_ready(cx) {
            Poll::Ready(ReadStatus::Terminate) => {
                log::trace!("read task is instructed to terminate");
                Poll::Ready(())
            }
            Poll::Ready(ReadStatus::Ready) => {
                let io = &this.io;
                let pool = this.state.memory_pool();
                let mut buf = self.state.get_read_buf();
                let (hw, lw) = pool.read_params().unpack();

                // read data from socket
                let mut new_bytes = 0;
                loop {
                    // make sure we've got room
                    let remaining = buf.remaining_mut();
                    if remaining < lw {
                        buf.reserve(hw - remaining);
                    }

                    match io.poll_read_buf(cx, &mut buf) {
                        Poll::Pending => {
                            log::trace!("no more data in io stream, read: {:?}", new_bytes);
                            break;
                        }
                        Poll::Ready(Ok(n)) => {
                            if n == 0 {
                                log::trace!("io stream is disconnected");
                                let _ = this.state.release_read_buf(buf, new_bytes);
                                this.state.close(None);
                                return Poll::Ready(());
                            } else {
                                new_bytes += n;
                                if buf.len() > hw {
                                    break;
                                }
                            }
                        }
                        Poll::Ready(Err(err)) => {
                            log::trace!("read task failed on io {:?}", err);
                            let _ = this.state.release_read_buf(buf, new_bytes);
                            this.state.close(Some(err));
                            return Poll::Ready(());
                        }
                    }
                }

                let _ = this.state.release_read_buf(buf, new_bytes);
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        }
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
        let mut this = self.as_mut().get_mut();

        match this.st {
            IoWriteState::Processing(ref mut delay) => {
                match this.state.poll_ready(cx) {
                    Poll::Ready(WriteStatus::Ready) => {
                        // flush framed instance
                        match flush_io(&this.io, &this.state, cx) {
                            Poll::Pending | Poll::Ready(true) => Poll::Pending,
                            Poll::Ready(false) => Poll::Ready(()),
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
                                Poll::Ready(true) => {
                                    *st = Shutdown::Flushed;
                                    continue;
                                }
                                Poll::Ready(false) => {
                                    log::trace!(
                                        "write task is closed with err during flush"
                                    );
                                    this.state.close(None);
                                    return Poll::Ready(());
                                }
                                _ => (),
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
                                let mut buf = BytesMut::new();
                                match io.poll_read_buf(cx, &mut buf) {
                                    Poll::Ready(Err(e)) => {
                                        this.state.close(Some(e));
                                        log::trace!("write task is stopped");
                                        return Poll::Ready(());
                                    }
                                    Poll::Ready(Ok(n)) if n == 0 => {
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
) -> Poll<bool> {
    let mut buf = if let Some(buf) = state.get_write_buf() {
        buf
    } else {
        return Poll::Ready(true);
    };
    let len = buf.len();

    if len != 0 {
        log::trace!("flushing framed transport: {}", len);

        let mut written = 0;
        while written < len {
            match io.poll_write_buf(cx, &buf[written..]) {
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        log::trace!("disconnected during flush, written {}", written);
                        let _ = state.release_write_buf(buf);
                        state.close(Some(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write frame to transport",
                        )));
                        return Poll::Ready(false);
                    } else {
                        written += n
                    }
                }
                Poll::Pending => break,
                Poll::Ready(Err(e)) => {
                    log::trace!("error during flush: {}", e);
                    let _ = state.release_write_buf(buf);
                    state.close(Some(e));
                    return Poll::Ready(false);
                }
            }
        }
        log::trace!("flushed {} bytes", written);

        // remove written data
        if written == len {
            buf.clear();
            let _ = state.release_write_buf(buf);
            Poll::Ready(true)
        } else {
            buf.advance(written);
            let _ = state.release_write_buf(buf);
            Poll::Pending
        }
    } else {
        Poll::Ready(true)
    }
}

#[cfg(any(feature = "tokio", feature = "tokio-traits"))]
mod tokio_impl {
    use tok_io::io::{AsyncRead, AsyncWrite, ReadBuf};

    use super::*;

    impl AsyncRead for IoTest {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let guard = self.local.lock().unwrap();
            let mut ch = guard.borrow_mut();
            *ch.waker.0.lock().unwrap().borrow_mut() = Some(cx.waker().clone());

            if !ch.buf.is_empty() {
                let size = std::cmp::min(ch.buf.len(), buf.remaining());
                let b = ch.buf.split_to(size);
                buf.put_slice(&b);
                return Poll::Ready(Ok(()));
            }

            match mem::take(&mut ch.read) {
                IoTestState::Ok => Poll::Pending,
                IoTestState::Close => {
                    ch.read = IoTestState::Close;
                    Poll::Ready(Ok(()))
                }
                IoTestState::Pending => Poll::Pending,
                IoTestState::Err(e) => Poll::Ready(Err(e)),
            }
        }
    }

    impl AsyncWrite for IoTest {
        fn poll_write(
            self: Pin<&mut Self>,
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

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            self.local
                .lock()
                .unwrap()
                .borrow_mut()
                .flags
                .insert(IoTestFlags::CLOSED);
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
#[allow(clippy::redundant_clone)]
mod tests {
    use super::*;

    #[ntex::test]
    async fn basic() {
        let (client, server) = IoTest::create();
        assert_eq!(client.tp, Type::Client);
        assert_eq!(client.clone().tp, Type::ClientClone);
        assert_eq!(server.tp, Type::Server);
        assert_eq!(server.clone().tp, Type::ServerClone);

        assert!(!server.is_client_dropped());
        drop(client);
        assert!(server.is_client_dropped());

        let server2 = server.clone();
        assert!(!server2.is_server_dropped());
        drop(server);
        assert!(server2.is_server_dropped());
    }
}
