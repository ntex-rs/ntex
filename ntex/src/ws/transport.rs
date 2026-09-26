//! Binary-stream adaptation for WebSocket connections.
use std::{cell::Cell, io, task::Poll};

use crate::codec::{Decoder, Encoder};
use crate::io::{Filter, FilterBuf, FilterLayer, Io, Layer};
use crate::service::{Ctx, Service};

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
/// I/O filter that exposes binary WebSocket messages as a byte stream.
///
/// Incoming binary messages and fragments are forwarded as bytes. Text
/// messages are rejected, ping frames receive automatic pong replies, and
/// close frames close the underlying I/O stream.
pub struct WsTransport {
    codec: Codec,
    flags: Cell<Flags>,
}

impl WsTransport {
    /// Adds a binary WebSocket transport filter to `io`.
    pub fn create<F: Filter>(io: Io<F>, codec: Codec) -> Io<Layer<WsTransport, F>> {
        io.add_filter(WsTransport {
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
            Err(io::Error::new(io::ErrorKind::InvalidData, err_message))
        }
    }
}

impl FilterLayer for WsTransport {
    #[inline]
    fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        let flags = self.flags.get();
        if !flags.contains(Flags::CLOSED) {
            self.insert_flags(Flags::CLOSED);
            let code = if flags.contains(Flags::PROTO_ERR) {
                CloseCode::Protocol
            } else {
                CloseCode::Normal
            };
            let _ = buf.with_write_buffers(|_, w_dst| {
                self.codec.encode(
                    Message::Close(Some(CloseReason {
                        code,
                        description: None,
                    })),
                    w_dst,
                )
            });
        }
        Ok(Poll::Ready(()))
    }

    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_read_buffers(|r_src, dst| {
            if let Some(src) = r_src {
                loop {
                    let Some(frame) = self.codec.decode(src).map_err(|e| {
                        log::trace!("Failed to decode ws codec frames: {e:?}");
                        self.insert_flags(Flags::PROTO_ERR);
                        io::Error::new(io::ErrorKind::InvalidData, e)
                    })?
                    else {
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
                                io::ErrorKind::InvalidData,
                                "WebSocket Text continuation frames are not supported",
                            ));
                        }
                        Frame::Text(_) => {
                            self.insert_flags(Flags::PROTO_ERR);
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "WebSockets Text frames are not supported",
                            ));
                        }
                        Frame::Ping(msg) => {
                            buf.with_write_buffers(|_, w_dst| {
                                let _ = self.codec.encode(Message::Pong(msg), w_dst);
                            });
                        }
                        Frame::Pong(_) => (),
                        Frame::Close(_) => {
                            buf.io().close();
                            break;
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|w_src, w_dst| -> Result<(), super::error::ProtocolError> {
            while let Some(page) = w_src.take() {
                self.codec.encode_page(page, w_dst)?;
            }
            Ok(())
        })
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }
}

#[derive(Clone, Debug)]
/// Service that adds a [`WsTransport`] filter to an I/O stream.
pub struct WsTransportService {
    codec: Codec,
}

impl WsTransportService {
    /// Creates a transport service using `codec`.
    pub fn new(codec: Codec) -> Self {
        Self { codec }
    }
}

impl<F: Filter> Service<(), Io<F>> for WsTransportService {
    type Res = Io<Layer<WsTransport, F>>;
    type Error = io::Error;

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, ()>) -> Result<Self::Res, Self::Error> {
        Ok(WsTransport::create(io, self.codec.clone()))
    }
}
