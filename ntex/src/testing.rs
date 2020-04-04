use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::{io, time};

use bytes::BytesMut;
use futures::future::poll_fn;
use futures::task::AtomicWaker;

use crate::codec::{AsyncRead, AsyncWrite};
use crate::rt::time::delay_for;

/// Async io stream
pub struct Io {
    tp: Type,
    state: Arc<Cell<State>>,
    read: Arc<Mutex<RefCell<Channel>>>,
    write: Arc<Mutex<RefCell<Channel>>>,
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
    client_closed: bool,
    server_closed: bool,
}

#[derive(Default)]
struct Channel {
    buf: BytesMut,
    read_err: Option<io::Error>,
    read_waker: AtomicWaker,
    read_close: CloseState,
    write_err: Option<io::Error>,
    write_waker: AtomicWaker,
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
        let left = Arc::new(Mutex::new(RefCell::new(Channel::default())));
        let right = Arc::new(Mutex::new(RefCell::new(Channel::default())));
        let state = Arc::new(Cell::new(State::default()));

        (
            Io {
                tp: Type::Client,
                read: left.clone(),
                write: right.clone(),
                state: state.clone(),
            },
            Io {
                state,
                tp: Type::Server,
                read: right,
                write: left,
            },
        )
    }

    pub fn is_client_closed(&self) -> bool {
        self.state.get().client_closed
    }

    pub fn is_server_closed(&self) -> bool {
        self.state.get().server_closed
    }

    /// Access read buffer.
    pub fn read_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let guard = self.read.lock().unwrap();
        let mut ch = guard.borrow_mut();
        f(&mut ch.buf)
    }

    /// Access write buffer.
    pub async fn close(&self) {
        {
            let guard = self.write.lock().unwrap();
            let mut write = guard.borrow_mut();
            write.read_close = CloseState::Closed;
            write.read_waker.wake();
        }
        delay_for(time::Duration::from_millis(35)).await;
    }

    /// Access write buffer.
    pub fn write_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let guard = self.write.lock().unwrap();
        let mut ch = guard.borrow_mut();
        f(&mut ch.buf)
    }

    /// Add extra data to the buffer and notify reader
    pub fn write<T: AsRef<[u8]>>(&self, data: T) {
        let guard = self.write.lock().unwrap();
        let mut write = guard.borrow_mut();
        write.buf.extend_from_slice(data.as_ref());
        write.read_waker.wake();
    }

    /// Read any available data
    pub fn read_any(&self) -> BytesMut {
        self.read.lock().unwrap().borrow_mut().buf.split()
    }

    /// Read data, if data is not available wait for it
    pub async fn read(&self) -> Result<BytesMut, io::Error> {
        if self.read.lock().unwrap().borrow().buf.is_empty() {
            poll_fn(|cx| {
                let guard = self.read.lock().unwrap();
                let read = guard.borrow_mut();
                if read.buf.is_empty() {
                    let closed = match self.tp {
                        Type::Client | Type::ClientClone => self.is_server_closed(),
                        Type::Server | Type::ServerClone => self.is_client_closed(),
                    };
                    if closed {
                        Poll::Ready(())
                    } else {
                        read.read_waker.register(cx.waker());
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
        Ok(self.read.lock().unwrap().borrow_mut().buf.split())
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
            read: self.read.clone(),
            write: self.write.clone(),
            state: self.state.clone(),
        }
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        let mut state = self.state.get();
        match self.tp {
            Type::Server => state.server_closed = true,
            Type::Client => state.client_closed = true,
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
        let guard = self.read.lock().unwrap();
        let mut ch = guard.borrow_mut();
        ch.read_waker.register(cx.waker());

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
        let guard = self.write.lock().unwrap();
        let mut ch = guard.borrow_mut();

        if let Some(err) = ch.write_err.take() {
            Poll::Ready(Err(err))
        } else {
            ch.write_waker.wake();
            ch.buf.extend(buf);
            Poll::Ready(Ok(buf.len()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
