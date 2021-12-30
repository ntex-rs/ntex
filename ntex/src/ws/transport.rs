//! An implementation of WebSockets base bytes streams
use std::{any, cell::Cell, cmp, io, task::Context, task::Poll};

use crate::codec::{Decoder, Encoder};
use crate::io::{Base, Filter, FilterFactory, Io, IoRef, ReadStatus, WriteStatus};
use crate::util::{BufMut, BytesMut, PoolRef, Ready};

use super::{Codec, Frame, Item, Message};

bitflags::bitflags! {
    struct Flags: u8  {
        const CLOSED       = 0b0001;
        const CONTINUATION = 0b0010;
    }
}

/// An implementation of WebSockets streams
pub struct WsTransport<F = Base> {
    inner: F,
    pool: PoolRef,
    codec: Codec,
    flags: Cell<Flags>,
    read_buf: Cell<Option<BytesMut>>,
}

impl<F: Filter> WsTransport<F> {
    fn insert_flags(&self, flags: Flags) {
        let mut f = self.flags.get();
        f.insert(flags);
        self.flags.set(f);
    }

    fn remove_flags(&self, flags: Flags) {
        let mut f = self.flags.get();
        f.remove(flags);
        self.flags.set(f);
    }

    fn continuation_must_start(&self, err_message: &'static str) -> io::Result<()> {
        if self.flags.get().contains(Flags::CONTINUATION) {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Other, err_message))
        }
    }
}

impl<F: Filter> Filter for WsTransport<F> {
    #[inline]
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        self.inner.query(id)
    }

    #[inline]
    fn poll_shutdown(&self) -> Poll<io::Result<()>> {
        if !self.flags.get().contains(Flags::CLOSED) {
            self.insert_flags(Flags::CLOSED);
            let mut b = self
                .inner
                .get_write_buf()
                .unwrap_or_else(|| self.pool.get_write_buf());
            let _ = self.codec.encode(Message::Close(None), &mut b);
            self.release_write_buf(b)?;
        }

        self.inner.poll_shutdown()
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        self.inner.poll_read_ready(cx)
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        self.inner.poll_write_ready(cx)
    }

    #[inline]
    fn get_read_buf(&self) -> Option<BytesMut> {
        self.inner.get_read_buf().or_else(|| self.read_buf.take())
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesMut> {
        None
    }

    fn release_read_buf(
        &self,
        io: &IoRef,
        src: BytesMut,
        dst: &mut Option<BytesMut>,
        nbytes: usize,
    ) -> io::Result<usize> {
        // store to read_buf
        let mut src = {
            let mut dst = None;
            let result = self.inner.release_read_buf(io, src, &mut dst, nbytes);
            if let Err(err) = result {
                io.want_shutdown(Some(err));
            }
            if let Some(dst) = dst {
                dst
            } else {
                return Ok(0);
            }
        };
        let (hw, lw) = self.pool.read_params().unpack();

        // get inner filter buffer
        if dst.is_none() {
            *dst = Some(self.pool.get_read_buf());
        }
        let buf = dst.as_mut().unwrap();
        let buf_len = buf.len();

        loop {
            // make sure we've got room
            let remaining = buf.remaining_mut();
            if remaining < lw {
                buf.reserve(hw - remaining);
            }

            let frame = if let Some(frame) = self.codec.decode(&mut src).map_err(|e| {
                log::trace!("Failed to decode ws codec frames: {:?}", e);
                io::Error::new(io::ErrorKind::Other, e)
            })? {
                frame
            } else {
                break;
            };

            match frame {
                Frame::Binary(bin) => buf.extend_from_slice(&bin),
                Frame::Continuation(Item::FirstBinary(bin)) => {
                    self.insert_flags(Flags::CONTINUATION);
                    buf.extend_from_slice(&bin);
                }
                Frame::Continuation(Item::Continue(bin)) => {
                    self.continuation_must_start("Continuation frame is not started")?;
                    buf.extend_from_slice(&bin);
                }
                Frame::Continuation(Item::Last(bin)) => {
                    self.continuation_must_start(
                        "Continuation frame is not started, last frame is received",
                    )?;
                    buf.extend_from_slice(&bin);
                    self.remove_flags(Flags::CONTINUATION);
                }
                Frame::Continuation(Item::FirstText(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "WebSocket Text continuation frames are not supported",
                    ));
                }
                Frame::Text(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "WebSockets Text frames are not supported",
                    ));
                }
                Frame::Ping(msg) => {
                    let mut b = self
                        .inner
                        .get_write_buf()
                        .unwrap_or_else(|| self.pool.get_write_buf());
                    let _ = self.codec.encode(Message::Pong(msg), &mut b);
                    self.release_write_buf(b)?;
                }
                Frame::Pong(_) => (),
                Frame::Close(_) => {
                    io.want_shutdown(None);
                    break;
                }
            };
        }

        if src.is_empty() {
            self.pool.release_read_buf(src);
        } else {
            self.read_buf.set(Some(src));
        }
        Ok(buf.len() - buf_len)
    }

    fn release_write_buf(&self, src: BytesMut) -> Result<(), io::Error> {
        let mut buf = if let Some(buf) = self.inner.get_write_buf() {
            buf
        } else {
            self.pool.get_write_buf()
        };

        // make sure we've got room
        let (hw, lw) = self.pool.write_params().unpack();
        let remaining = buf.remaining_mut();
        if remaining < lw {
            buf.reserve(cmp::max(hw, buf.len() + 12) - remaining);
        }

        // Encoder ws::Codec do not fail
        let _ = self.codec.encode(Message::Binary(src.freeze()), &mut buf);
        self.inner.release_write_buf(buf)
    }
}

/// WebSockets transport factory
pub struct WsTransportFactory {
    codec: Codec,
}

impl WsTransportFactory {
    /// Create websockets transport factory
    pub fn new(codec: Codec) -> Self {
        Self { codec }
    }
}

impl<F: Filter> FilterFactory<F> for WsTransportFactory {
    type Filter = WsTransport<F>;

    type Error = io::Error;
    type Future = Ready<Io<Self::Filter>, Self::Error>;

    fn create(self, st: Io<F>) -> Self::Future {
        let pool = st.memory_pool();

        Ready::from(st.map_filter(|inner: F| {
            let read_buf = inner.get_read_buf();
            Ok(WsTransport {
                pool,
                inner,
                codec: self.codec,
                flags: Cell::new(Flags::empty()),
                read_buf: Cell::new(read_buf),
            })
        }))
    }
}
