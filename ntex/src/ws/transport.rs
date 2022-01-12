//! An implementation of WebSockets base bytes streams
use std::{any, cell::Cell, cmp, io, task::Context, task::Poll};

use crate::codec::{Decoder, Encoder};
use crate::io::{Base, Filter, FilterFactory, Io, IoRef, ReadStatus, WriteStatus};
use crate::util::{BufMut, BytesMut, PoolRef, Ready};

use super::{CloseCode, CloseReason, Codec, Frame, Item, Message};

bitflags::bitflags! {
    struct Flags: u8  {
        const CLOSED       = 0b0001;
        const CONTINUATION = 0b0010;
        const PROTO_ERR    = 0b0100;
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
            self.insert_flags(Flags::PROTO_ERR);
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
        let flags = self.flags.get();
        if !flags.contains(Flags::CLOSED) {
            self.insert_flags(Flags::CLOSED);
            let mut b = self
                .inner
                .get_write_buf()
                .unwrap_or_else(|| self.pool.get_write_buf());
            let code = if flags.contains(Flags::PROTO_ERR) {
                CloseCode::Protocol
            } else {
                CloseCode::Normal
            };
            let _ = self.codec.encode(
                Message::Close(Some(CloseReason {
                    code,
                    description: None,
                })),
                &mut b,
            );
            self.inner.release_write_buf(b)?;
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
        self.read_buf.take()
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesMut> {
        None
    }

    #[inline]
    fn release_read_buf(&self, buf: BytesMut) {
        self.read_buf.set(Some(buf));
    }

    fn process_read_buf(&self, io: &IoRef, nbytes: usize) -> io::Result<(usize, usize)> {
        // ask inner filter to process read buf
        match self.inner.process_read_buf(io, nbytes) {
            Err(err) => io.want_shutdown(Some(err)),
            Ok((_, 0)) => return Ok((0, 0)),
            Ok(_) => (),
        }

        // get inner buffer
        let mut src = if let Some(src) = self.inner.get_read_buf() {
            src
        } else {
            return Ok((0, 0));
        };

        // get processed buffer
        let mut dst = if let Some(dst) = self.read_buf.take() {
            dst
        } else {
            self.pool.get_read_buf()
        };
        let dst_len = dst.len();
        let (hw, lw) = self.pool.read_params().unpack();

        loop {
            // make sure we've got room
            let remaining = dst.remaining_mut();
            if remaining < lw {
                dst.reserve(hw - remaining);
            }

            let frame = if let Some(frame) = self.codec.decode(&mut src).map_err(|e| {
                log::trace!("Failed to decode ws codec frames: {:?}", e);
                self.insert_flags(Flags::PROTO_ERR);
                io::Error::new(io::ErrorKind::Other, e)
            })? {
                frame
            } else {
                break;
            };

            match frame {
                Frame::Binary(bin) => dst.extend_from_slice(&bin),
                Frame::Continuation(Item::FirstBinary(bin)) => {
                    self.insert_flags(Flags::CONTINUATION);
                    dst.extend_from_slice(&bin);
                }
                Frame::Continuation(Item::Continue(bin)) => {
                    self.continuation_must_start("Continuation frame is not started")?;
                    dst.extend_from_slice(&bin);
                }
                Frame::Continuation(Item::Last(bin)) => {
                    self.continuation_must_start(
                        "Continuation frame is not started, last frame is received",
                    )?;
                    dst.extend_from_slice(&bin);
                    self.remove_flags(Flags::CONTINUATION);
                }
                Frame::Continuation(Item::FirstText(_)) => {
                    self.insert_flags(Flags::PROTO_ERR);
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "WebSocket Text continuation frames are not supported",
                    ));
                }
                Frame::Text(_) => {
                    self.insert_flags(Flags::PROTO_ERR);
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

        let dlen = dst.len();
        let nbytes = dlen - dst_len;

        if src.is_empty() {
            self.pool.release_read_buf(src);
        } else {
            self.inner.release_read_buf(src);
        }
        self.read_buf.set(Some(dst));
        Ok((dlen, nbytes))
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
            Ok(WsTransport {
                pool,
                inner,
                codec: self.codec,
                flags: Cell::new(Flags::empty()),
                read_buf: Cell::new(None),
            })
        }))
    }
}
