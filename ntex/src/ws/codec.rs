use std::cell::Cell;

use crate::codec::{Decoder, Encoder};
use crate::util::{ByteString, Bytes, BytesMut};

use super::frame::Parser;
use super::proto::{CloseReason, OpCode};
use super::ProtocolError;

/// WebSocket message
#[derive(Debug, PartialEq)]
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
#[derive(Debug, PartialEq)]
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
#[derive(Debug, PartialEq)]
pub enum Item {
    FirstText(Bytes),
    FirstBinary(Bytes),
    Continue(Bytes),
    Last(Bytes),
}

#[derive(Debug, Clone)]
/// WebSockets protocol codec
pub struct Codec {
    flags: Cell<Flags>,
    max_size: usize,
}

bitflags::bitflags! {
    struct Flags: u8 {
        const SERVER         = 0b0000_0001;
        const R_CONTINUATION = 0b0000_0010;
        const W_CONTINUATION = 0b0000_0100;
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
                txt,
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
            Message::Close(reason) => Parser::write_close(
                dst,
                reason,
                !self.flags.get().contains(Flags::SERVER),
            ),
            Message::Continuation(cont) => match cont {
                Item::FirstText(data) => {
                    if self.flags.get().contains(Flags::W_CONTINUATION) {
                        return Err(ProtocolError::ContinuationStarted);
                    } else {
                        self.insert_flags(Flags::W_CONTINUATION);
                        Parser::write_message(
                            dst,
                            &data[..],
                            OpCode::Text,
                            false,
                            !self.flags.get().contains(Flags::SERVER),
                        )
                    }
                }
                Item::FirstBinary(data) => {
                    if self.flags.get().contains(Flags::W_CONTINUATION) {
                        return Err(ProtocolError::ContinuationStarted);
                    } else {
                        self.insert_flags(Flags::W_CONTINUATION);
                        Parser::write_message(
                            dst,
                            &data[..],
                            OpCode::Binary,
                            false,
                            !self.flags.get().contains(Flags::SERVER),
                        )
                    }
                }
                Item::Continue(data) => {
                    if self.flags.get().contains(Flags::W_CONTINUATION) {
                        Parser::write_message(
                            dst,
                            &data[..],
                            OpCode::Continue,
                            false,
                            !self.flags.get().contains(Flags::SERVER),
                        )
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
                        )
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
        match Parser::parse(src, self.flags.get().contains(Flags::SERVER), self.max_size)
        {
            Ok(Some((finished, opcode, payload))) => {
                // handle continuation
                if !finished {
                    return match opcode {
                        OpCode::Continue => {
                            if self.flags.get().contains(Flags::R_CONTINUATION) {
                                Ok(Some(Frame::Continuation(Item::Continue(
                                    payload
                                        .map(|pl| pl.freeze())
                                        .unwrap_or_else(Bytes::new),
                                ))))
                            } else {
                                Err(ProtocolError::ContinuationNotStarted)
                            }
                        }
                        OpCode::Binary => {
                            if !self.flags.get().contains(Flags::R_CONTINUATION) {
                                self.insert_flags(Flags::R_CONTINUATION);
                                Ok(Some(Frame::Continuation(Item::FirstBinary(
                                    payload
                                        .map(|pl| pl.freeze())
                                        .unwrap_or_else(Bytes::new),
                                ))))
                            } else {
                                Err(ProtocolError::ContinuationStarted)
                            }
                        }
                        OpCode::Text => {
                            if !self.flags.get().contains(Flags::R_CONTINUATION) {
                                self.insert_flags(Flags::R_CONTINUATION);
                                Ok(Some(Frame::Continuation(Item::FirstText(
                                    payload
                                        .map(|pl| pl.freeze())
                                        .unwrap_or_else(Bytes::new),
                                ))))
                            } else {
                                Err(ProtocolError::ContinuationStarted)
                            }
                        }
                        _ => {
                            error!("Unfinished fragment {:?}", opcode);
                            Err(ProtocolError::ContinuationFragment(opcode))
                        }
                    };
                }

                match opcode {
                    OpCode::Continue => {
                        if self.flags.get().contains(Flags::R_CONTINUATION) {
                            self.remove_flags(Flags::R_CONTINUATION);
                            Ok(Some(Frame::Continuation(Item::Last(
                                payload.map(|pl| pl.freeze()).unwrap_or_else(Bytes::new),
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
                    OpCode::Ping => Ok(Some(Frame::Ping(
                        payload.map(|pl| pl.freeze()).unwrap_or_else(Bytes::new),
                    ))),
                    OpCode::Pong => Ok(Some(Frame::Pong(
                        payload.map(|pl| pl.freeze()).unwrap_or_else(Bytes::new),
                    ))),
                    OpCode::Binary => Ok(Some(Frame::Binary(
                        payload.map(|pl| pl.freeze()).unwrap_or_else(Bytes::new),
                    ))),
                    OpCode::Text => Ok(Some(Frame::Text(
                        payload.map(|pl| pl.freeze()).unwrap_or_else(Bytes::new),
                    ))),
                }
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
