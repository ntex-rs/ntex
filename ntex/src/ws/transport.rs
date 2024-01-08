//! An implementation of WebSockets base bytes streams
use std::{cell::Cell, cmp, io, task::Poll};

use crate::codec::{Decoder, Encoder};
use crate::io::{Filter, FilterLayer, Io, Layer, ReadBuf, WriteBuf};
use crate::service::{Service, ServiceCtx};
use crate::util::{BufMut, PoolRef};

use super::{CloseCode, CloseReason, Codec, Frame, Item, Message};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8  {
        const CLOSED       = 0b0001;
        const CONTINUATION = 0b0010;
        const PROTO_ERR    = 0b0100;
    }
}

#[derive(Clone, Debug)]
/// An implementation of WebSockets streams
pub struct WsTransport {
    pool: PoolRef,
    codec: Codec,
    flags: Cell<Flags>,
}

impl WsTransport {
    /// Create websockets transport
    pub fn create<F: Filter>(io: Io<F>, codec: Codec) -> Io<Layer<WsTransport, F>> {
        let pool = io.memory_pool();

        io.add_filter(WsTransport {
            pool,
            codec,
            flags: Cell::new(Flags::empty()),
        })
    }

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
    fn shutdown(&self, buf: &WriteBuf<'_>) -> io::Result<Poll<()>> {
        let flags = self.flags.get();
        if !flags.contains(Flags::CLOSED) {
            self.insert_flags(Flags::CLOSED);
            let code = if flags.contains(Flags::PROTO_ERR) {
                CloseCode::Protocol
            } else {
                CloseCode::Normal
            };
            let _ = buf.with_dst(|buf| {
                self.codec.encode_vec(
                    Message::Close(Some(CloseReason {
                        code,
                        description: None,
                    })),
                    buf,
                )
            });
        }
        Ok(Poll::Ready(()))
    }

    fn process_read_buf(&self, buf: &ReadBuf<'_>) -> io::Result<usize> {
        if let Some(mut src) = buf.take_src() {
            let mut dst = buf.take_dst();
            let dst_len = dst.len();

            loop {
                // make sure we've got room
                self.pool.resize_read_buf(&mut dst);

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
                            b.with_dst(|b| self.codec.encode_vec(Message::Pong(msg), b))
                        });
                    }
                    Frame::Pong(_) => (),
                    Frame::Close(_) => {
                        buf.want_shutdown();
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

    fn process_write_buf(&self, buf: &WriteBuf<'_>) -> io::Result<()> {
        if let Some(src) = buf.take_src() {
            buf.with_dst(|dst| {
                // make sure we've got room
                let (hw, lw) = self.pool.write_params().unpack();
                let remaining = dst.remaining_mut();
                if remaining < lw {
                    dst.reserve(cmp::max(hw, dst.len() + 12) - remaining);
                }

                // Encoder ws::Codec do not fail
                let _ = self.codec.encode_vec(Message::Binary(src.freeze()), dst);
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
/// WebSockets transport service
pub struct WsTransportService {
    codec: Codec,
}

impl WsTransportService {
    /// Create websockets transport service
    pub fn new(codec: Codec) -> Self {
        Self { codec }
    }
}

impl<F: Filter> Service<Io<F>> for WsTransportService {
    type Response = Io<Layer<WsTransport, F>>;
    type Error = io::Error;

    async fn call(
        &self,
        io: Io<F>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        Ok(WsTransport::create(io, self.codec.clone()))
    }
}
