use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::{cmp, fmt, io, mem, pin::Pin, time};

use bytes::BytesMut;

use crate::codec::{AsyncRead, AsyncWrite, ReadBuf};
use crate::rt::time::sleep;
use crate::util::poll_fn;

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
pub struct Io {
    tp: Type,
    state: Arc<Cell<State>>,
    local: Arc<Mutex<RefCell<Channel>>>,
    remote: Arc<Mutex<RefCell<Channel>>>,
}

bitflags::bitflags! {
    struct Flags: u8 {
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
    flags: Flags,
    waker: AtomicWaker,
    read: IoState,
    write: IoState,
}

impl Channel {
    fn is_closed(&self) -> bool {
        self.flags.contains(Flags::CLOSED)
    }
}

impl Default for Flags {
    fn default() -> Self {
        Flags::empty()
    }
}

#[derive(Debug)]
enum IoState {
    Ok,
    Pending,
    Close,
    Err(io::Error),
}

impl Default for IoState {
    fn default() -> Self {
        IoState::Ok
    }
}

impl Io {
    /// Create a two interconnected streams
    pub fn create() -> (Io, Io) {
        let local = Arc::new(Mutex::new(RefCell::new(Channel::default())));
        let remote = Arc::new(Mutex::new(RefCell::new(Channel::default())));
        let state = Arc::new(Cell::new(State::default()));

        (
            Io {
                tp: Type::Client,
                local: local.clone(),
                remote: remote.clone(),
                state: state.clone(),
            },
            Io {
                state,
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

    /// Set read to Pending state
    pub fn read_pending(&self) {
        self.remote.lock().unwrap().borrow_mut().read = IoState::Pending;
    }

    /// Set read to error
    pub fn read_error(&self, err: io::Error) {
        self.remote.lock().unwrap().borrow_mut().read = IoState::Err(err);
    }

    /// Set write error on remote side
    pub fn write_error(&self, err: io::Error) {
        self.local.lock().unwrap().borrow_mut().write = IoState::Err(err);
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
            remote.read = IoState::Close;
            remote.waker.wake();
        }
        sleep(time::Duration::from_millis(35)).await;
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
}

impl Clone for Io {
    fn clone(&self) -> Self {
        let tp = match self.tp {
            Type::Server => Type::ServerClone,
            Type::Client => Type::ClientClone,
            val => val,
        };

        Io {
            tp,
            local: self.local.clone(),
            remote: self.remote.clone(),
            state: self.state.clone(),
        }
    }
}

impl Drop for Io {
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

impl AsyncRead for Io {
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
            IoState::Ok => Poll::Pending,
            IoState::Close => {
                ch.read = IoState::Close;
                Poll::Ready(Ok(()))
            }
            IoState::Pending => Poll::Pending,
            IoState::Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let guard = self.remote.lock().unwrap();
        let mut ch = guard.borrow_mut();

        match mem::take(&mut ch.write) {
            IoState::Ok => {
                let cap = cmp::min(buf.len(), ch.buf_cap);
                if cap > 0 {
                    ch.buf.extend(&buf[..cap]);
                    ch.buf_cap -= cap;
                    ch.flags.remove(Flags::FLUSHED);
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
            IoState::Close => Poll::Ready(Ok(0)),
            IoState::Pending => {
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
            IoState::Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.local
            .lock()
            .unwrap()
            .borrow_mut()
            .flags
            .insert(Flags::CLOSED);
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
#[allow(clippy::redundant_clone)]
mod tests {
    use super::*;

    #[crate::rt_test]
    async fn basic() {
        let (client, server) = Io::create();
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
