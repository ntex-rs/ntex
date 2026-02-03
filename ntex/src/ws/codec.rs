use std::cell::Cell;

use crate::codec::{Decoder, Encoder};
use crate::util::{ByteString, Bytes, BytesMut};

use super::error::ProtocolError;
use super::frame::Parser;
use super::proto::{CloseReason, OpCode};

/// WebSocket message
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Text message
    Text(ByteString),
    /// Binary message
    Binary(Bytes),
    /// Continuation
    Continuation(Item),
    /// Ping message
    Ping(Bytes),
    /// Pong message
    Pong(Bytes),
    /// Close message with optional reason
    Close(Option<CloseReason>),
}

/// WebSocket frame
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Text frame, codec does not verify utf8 encoding
    Text(Bytes),
    /// Binary frame
    Binary(Bytes),
    /// Continuation
    Continuation(Item),
    /// Ping message
    Ping(Bytes),
    /// Pong message
    Pong(Bytes),
    /// Close message with optional reason
    Close(Option<CloseReason>),
}

/// WebSocket continuation item
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    FirstText(Bytes),
    FirstBinary(Bytes),
    Continue(Bytes),
    Last(Bytes),
}

#[derive(Debug, Clone)]
/// `WebSockets` protocol codec
pub struct Codec {
    flags: Cell<Flags>,
    max_size: usize,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const SERVER         = 0b0000_0001;
        const R_CONTINUATION = 0b0000_0010;
        const W_CONTINUATION = 0b0000_0100;
        const CLOSED         = 0b0000_1000;
    }
}

impl Codec {
    /// Create new websocket frames decoder
    pub fn new() -> Codec {
        Codec {
            max_size: 65_536,
            flags: Cell::new(Flags::SERVER),
        }
    }

    /// Set max frame size
    ///
    /// By default max size is set to 64kb
    pub fn max_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    /// Set decoder to client mode.
    ///
    /// By default decoder works in server mode.
    pub fn client_mode(self) -> Self {
        self.remove_flags(Flags::SERVER);
        self
    }

    /// Check if codec encoded `Close` message
    pub fn is_closed(&self) -> bool {
        self.flags.get().contains(Flags::CLOSED)
    }

    fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    fn remove_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.remove(f);
        self.flags.set(flags);
    }
}

impl Default for Codec {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder for Codec {
    type Item = Message;
    type Error = ProtocolError;

    fn encode(&self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item {
            Message::Text(txt) => Parser::write_message(
                dst,
                txt.as_ref(),
                OpCode::Text,
                true,
                !self.flags.get().contains(Flags::SERVER),
            ),
            Message::Binary(bin) => Parser::write_message(
                dst,
                bin,
                OpCode::Binary,
                true,
                !self.flags.get().contains(Flags::SERVER),
            ),
            Message::Ping(txt) => Parser::write_message(
                dst,
                txt,
                OpCode::Ping,
                true,
                !self.flags.get().contains(Flags::SERVER),
            ),
            Message::Pong(txt) => Parser::write_message(
                dst,
                txt,
                OpCode::Pong,
                true,
                !self.flags.get().contains(Flags::SERVER),
            ),
            Message::Close(reason) => {
                self.insert_flags(Flags::CLOSED);
                Parser::write_close(dst, reason, !self.flags.get().contains(Flags::SERVER));
            }
            Message::Continuation(cont) => match cont {
                Item::FirstText(data) => {
                    if self.flags.get().contains(Flags::W_CONTINUATION) {
                        return Err(ProtocolError::ContinuationStarted);
                    }
                    self.insert_flags(Flags::W_CONTINUATION);
                    Parser::write_message(
                        dst,
                        &data[..],
                        OpCode::Text,
                        false,
                        !self.flags.get().contains(Flags::SERVER),
                    );
                }
                Item::FirstBinary(data) => {
                    if self.flags.get().contains(Flags::W_CONTINUATION) {
                        return Err(ProtocolError::ContinuationStarted);
                    }
                    self.insert_flags(Flags::W_CONTINUATION);
                    Parser::write_message(
                        dst,
                        &data[..],
                        OpCode::Binary,
                        false,
                        !self.flags.get().contains(Flags::SERVER),
                    );
                }
                Item::Continue(data) => {
                    if self.flags.get().contains(Flags::W_CONTINUATION) {
                        Parser::write_message(
                            dst,
                            &data[..],
                            OpCode::Continue,
                            false,
                            !self.flags.get().contains(Flags::SERVER),
                        );
                    } else {
                        return Err(ProtocolError::ContinuationNotStarted);
                    }
                }
                Item::Last(data) => {
                    if self.flags.get().contains(Flags::W_CONTINUATION) {
                        self.remove_flags(Flags::W_CONTINUATION);
                        Parser::write_message(
                            dst,
                            &data[..],
                            OpCode::Continue,
                            true,
                            !self.flags.get().contains(Flags::SERVER),
                        );
                    } else {
                        return Err(ProtocolError::ContinuationNotStarted);
                    }
                }
            },
        }
        Ok(())
    }
}

impl Decoder for Codec {
    type Item = Frame;
    type Error = ProtocolError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match Parser::parse(src, self.flags.get().contains(Flags::SERVER), self.max_size) {
            Ok(Some((finished, opcode, payload))) => {
                // handle continuation
                if finished {
                    match opcode {
                        OpCode::Continue => {
                            if self.flags.get().contains(Flags::R_CONTINUATION) {
                                self.remove_flags(Flags::R_CONTINUATION);
                                Ok(Some(Frame::Continuation(Item::Last(
                                    payload.unwrap_or_else(Bytes::new),
                                ))))
                            } else {
                                Err(ProtocolError::ContinuationNotStarted)
                            }
                        }
                        OpCode::Bad => Err(ProtocolError::BadOpCode),
                        OpCode::Close => {
                            if let Some(ref pl) = payload {
                                let close_reason = Parser::parse_close_payload(pl);
                                Ok(Some(Frame::Close(close_reason)))
                            } else {
                                Ok(Some(Frame::Close(None)))
                            }
                        }
                        OpCode::Ping => {
                            Ok(Some(Frame::Ping(payload.unwrap_or_else(Bytes::new))))
                        }
                        OpCode::Pong => {
                            Ok(Some(Frame::Pong(payload.unwrap_or_else(Bytes::new))))
                        }
                        OpCode::Binary => {
                            Ok(Some(Frame::Binary(payload.unwrap_or_else(Bytes::new))))
                        }
                        OpCode::Text => {
                            Ok(Some(Frame::Text(payload.unwrap_or_else(Bytes::new))))
                        }
                    }
                } else {
                    match opcode {
                        OpCode::Continue => {
                            if self.flags.get().contains(Flags::R_CONTINUATION) {
                                Ok(Some(Frame::Continuation(Item::Continue(
                                    payload.unwrap_or_else(Bytes::new),
                                ))))
                            } else {
                                Err(ProtocolError::ContinuationNotStarted)
                            }
                        }
                        OpCode::Binary => {
                            if self.flags.get().contains(Flags::R_CONTINUATION) {
                                Err(ProtocolError::ContinuationStarted)
                            } else {
                                self.insert_flags(Flags::R_CONTINUATION);
                                Ok(Some(Frame::Continuation(Item::FirstBinary(
                                    payload.unwrap_or_else(Bytes::new),
                                ))))
                            }
                        }
                        OpCode::Text => {
                            if self.flags.get().contains(Flags::R_CONTINUATION) {
                                Err(ProtocolError::ContinuationStarted)
                            } else {
                                self.insert_flags(Flags::R_CONTINUATION);
                                Ok(Some(Frame::Continuation(Item::FirstText(
                                    payload.unwrap_or_else(Bytes::new),
                                ))))
                            }
                        }
                        OpCode::Ping => {
                            Ok(Some(Frame::Ping(payload.unwrap_or_else(Bytes::new))))
                        }
                        OpCode::Pong => {
                            Ok(Some(Frame::Pong(payload.unwrap_or_else(Bytes::new))))
                        }
                        OpCode::Bad => Err(ProtocolError::BadOpCode),
                        OpCode::Close => {
                            log::error!("Unfinished fragment {opcode:?}");
                            Err(ProtocolError::ContinuationFragment(opcode))
                        }
                    }
                }
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
