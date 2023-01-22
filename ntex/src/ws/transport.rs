//! An implementation of WebSockets base bytes streams
use std::{cell::Cell, cmp, io, task::Poll};

use crate::codec::{Decoder, Encoder};
use crate::io::{Buffer, Filter, FilterFactory, FilterLayer, Io, Layer};
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

impl Filter for WsTransport {
    #[inline]
    fn shutdown(&self, buf: &mut Buffer<'_>) -> io::Result<Poll<()>> {
        let flags = self.flags.get();
        if !flags.contains(Flags::CLOSED) {
            self.insert_flags(Flags::CLOSED);
            let b = buf.get_write_dst();
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
                b,
            );
        }
    }

    fn process_read_buf(&self, buf: &mut Buffer<'_>, _: usize) -> io::Result<usize> {
        // get current and inner buffer
        let (src, dst) = buf.get_read_pair();
        let dst_len = dst.len();
        let (hw, lw) = self.pool.read_params().unpack();

        loop {
            // make sure we've got room
            let remaining = dst.remaining_mut();
            if remaining < lw {
                dst.reserve(hw - remaining);
            }

            let frame = if let Some(frame) = self.codec.decode_vec(src).map_err(|e| {
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
                    let _ = self
                        .codec
                        .encode_vec(Message::Pong(msg), buf.get_write_buf());
                    buf.process_write_buf(&self.inner)?;
                }
                Frame::Pong(_) => (),
                Frame::Close(_) => {
                    buf.io().want_shutdown(None);
                    break;
                }
            };
        }

        Ok(dst.len() - dst_len)
    }

    fn process_write_buf(&self, buf: &mut Buffer<'_>) -> io::Result<()> {
        let (src, dst) = buf.get_write_pair();

        if !src.is_empty() {
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
}

impl<F: FilterLayer> FilterFactory<F> for WsTransportFactory {
    type Filter = WsTransport;

    type Error = io::Error;
    type Future = Ready<Io<Layer<Self::Filter, F>>, Self::Error>;

    fn create(self, io: Io<F>) -> Self::Future {
        let pool = io.memory_pool();

        Ready::Ok(io.add_filter(WsTransport {
            pool,
            codec: self.codec,
            flags: Cell::new(Flags::empty()),
        }))
    }
}
