use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::{cmp, io, mem, time};

use bytes::BytesMut;
use futures::future::poll_fn;
use futures::task::AtomicWaker;

use crate::codec::{AsyncRead, AsyncWrite};
use crate::rt::time::delay_for;

/// Async io stream
pub struct Io {
    tp: Type,
    state: Arc<Cell<State>>,
    local: Arc<Mutex<RefCell<Channel>>>,
    remote: Arc<Mutex<RefCell<Channel>>>,
}

bitflags::bitflags! {
    struct Flags: u8 {
        const FLUSHED            = 0b0000_0001;
        const CLOSED             = 0b0000_0010;
    }
}

#[derive(Copy, Clone)]
enum Type {
    Client,
    Server,
    ClientClone,
    ServerClone,
}

#[derive(Copy, Clone, Default)]
struct State {
    client_dropped: bool,
    server_dropped: bool,
}

#[derive(Default)]
struct Channel {
    buf: BytesMut,
    buf_cap: usize,
    flags: Flags,
    waker: AtomicWaker,
    read_err: Option<io::Error>,
    read_close: CloseState,
    write: IoState,
    flush: IoState,
}

impl Channel {
    fn is_closed(&self) -> bool {
        self.flags.contains(Flags::CLOSED)
    }

    fn is_flushed(&self) -> bool {
        self.flags.contains(Flags::FLUSHED)
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
    Err(io::Error),
}

impl Default for IoState {
    fn default() -> Self {
        IoState::Ok
    }
}

enum CloseState {
    Opened,
    Closed,
}

impl Default for CloseState {
    fn default() -> Self {
        CloseState::Opened
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

    /// Check flushed state
    pub fn is_flushed(&self) -> bool {
        self.remote.lock().unwrap().borrow().is_flushed()
    }

    /// Access read buffer.
    pub fn read_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let guard = self.local.lock().unwrap();
        let mut ch = guard.borrow_mut();
        f(&mut ch.buf)
    }

    /// Access write buffer.
    pub async fn close(&self) {
        {
            let guard = self.remote.lock().unwrap();
            let mut write = guard.borrow_mut();
            write.read_close = CloseState::Closed;
            write.waker.wake();
        }
        delay_for(time::Duration::from_millis(35)).await;
    }

    /// Add extra data to the buffer and notify reader
    pub fn write<T: AsRef<[u8]>>(&self, data: T) {
        let guard = self.remote.lock().unwrap();
        let mut write = guard.borrow_mut();
        write.buf.extend_from_slice(data.as_ref());
        write.waker.wake();
    }

    /// Set flush to Pending state
    pub fn flush_pending(&self) {
        self.remote.lock().unwrap().borrow_mut().flush = IoState::Pending;
    }

    /// Set flush to errore
    pub fn flush_error(&self, err: io::Error) {
        self.remote.lock().unwrap().borrow_mut().flush = IoState::Err(err);
    }

    /// Read any available data
    pub fn remote_buffer_cap(&self, cap: usize) {
        self.local.lock().unwrap().borrow_mut().buf_cap = cap;
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
                        read.waker.register(cx.waker());
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
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let guard = self.local.lock().unwrap();
        let mut ch = guard.borrow_mut();
        ch.waker.register(cx.waker());

        let result = if ch.buf.is_empty() {
            if let Some(err) = ch.read_err.take() {
                Err(err)
            } else {
                return match ch.read_close {
                    CloseState::Opened => Poll::Pending,
                    CloseState::Closed => Poll::Ready(Ok(0)),
                };
            }
        } else {
            let size = std::cmp::min(ch.buf.len(), buf.len());
            let b = ch.buf.split_to(size);
            buf[..size].copy_from_slice(&b);
            Ok(size)
        };

        Poll::Ready(result)
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
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
                    Poll::Pending
                }
            }
            IoState::Pending => Poll::Pending,
            IoState::Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let guard = self.local.lock().unwrap();
        let mut ch = guard.borrow_mut();

        match mem::take(&mut ch.flush) {
            IoState::Ok => {
                ch.flags.insert(Flags::FLUSHED);
                Poll::Ready(Ok(()))
            }
            IoState::Pending => Poll::Pending,
            IoState::Err(e) => Poll::Ready(Err(e)),
        }
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
