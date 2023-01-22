//! An implementation of WebSockets base bytes streams
use std::{cell::Cell, cmp, io, task::Poll};

use crate::codec::{Decoder, Encoder};
use crate::io::{Filter, FilterFactory, FilterLayer, Io, Layer, ReadBuf, WriteBuf};
use crate::util::{BufMut, PoolRef, Ready};

use super::{CloseCode, CloseReason, Codec, Frame, Item, Message};

bitflags::bitflags! {
    struct Flags: u8  {
        const CLOSED       = 0b0001;
        const CONTINUATION = 0b0010;
        const PROTO_ERR    = 0b0100;
    }
}

/// An implementation of WebSockets streams
pub struct WsTransport {
    pool: PoolRef,
    codec: Codec,
    flags: Cell<Flags>,
}

impl WsTransport {
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

impl FilterLayer for WsTransport {
    #[inline]
    fn shutdown(&self, buf: &mut WriteBuf<'_>) -> io::Result<Poll<()>> {
        let flags = self.flags.get();
        if !flags.contains(Flags::CLOSED) {
            self.insert_flags(Flags::CLOSED);
            let code = if flags.contains(Flags::PROTO_ERR) {
                CloseCode::Protocol
            } else {
                CloseCode::Normal
            };
            let _ = self.codec.encode_vec(
                Message::Close(Some(CloseReason {
                    code,
                    description: None,
                })),
                buf.get_dst(),
            );
        }
        Ok(Poll::Ready(()))
    }

    fn process_read_buf(&self, buf: &mut ReadBuf<'_>) -> io::Result<usize> {
        if let Some(mut src) = buf.take_src() {
            let mut dst = buf.take_dst();
            let dst_len = dst.len();
            let (hw, lw) = self.pool.read_params().unpack();

            loop {
                // make sure we've got room
                let remaining = dst.remaining_mut();
                if remaining < lw {
                    dst.reserve(hw - remaining);
                }

                let frame = if let Some(frame) =
                    self.codec.decode_vec(&mut src).map_err(|e| {
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
                        let _ = buf.with_write_buf(|b| {
                            self.codec.encode_vec(Message::Pong(msg), b.get_dst())
                        });
                    }
                    Frame::Pong(_) => (),
                    Frame::Close(_) => {
                        buf.io().want_shutdown(None);
                        break;
                    }
                };
            }

            let nb = dst.len() - dst_len;
            buf.set_dst(Some(dst));
            buf.set_src(Some(src));
            Ok(nb)
        } else {
            Ok(0)
        }
    }

    fn process_write_buf(&self, buf: &mut WriteBuf<'_>) -> io::Result<()> {
        if let Some(src) = buf.take_src() {
            let dst = buf.get_dst();

            // make sure we've got room
            let (hw, lw) = self.pool.write_params().unpack();
            let remaining = dst.remaining_mut();
            if remaining < lw {
                dst.reserve(cmp::max(hw, dst.len() + 12) - remaining);
            }

            // Encoder ws::Codec do not fail
            let _ = self.codec.encode_vec(Message::Binary(src.freeze()), dst);
        }
        Ok(())
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

    /// Create websockets transport
    pub fn transport<F: Filter>(self, io: Io<F>) -> Io<Layer<WsTransport, F>> {
        let pool = io.memory_pool();

        io.add_filter(WsTransport {
            pool,
            codec: self.codec,
            flags: Cell::new(Flags::empty()),
        })
    }
}

impl<F: Filter> FilterFactory<F> for WsTransportFactory {
    type Filter = WsTransport;

    type Error = io::Error;
    type Future = Ready<Io<Layer<Self::Filter, F>>, Self::Error>;

    fn create(self, io: Io<F>) -> Self::Future {
        Ready::Ok(self.transport(io))
    }
}
