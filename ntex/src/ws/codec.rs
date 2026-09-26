use std::cell::Cell;

use crate::codec::{Decoder, Encoder};
use crate::util::{BytePage, BytePages, ByteString, Bytes, BytesMut};

use super::error::ProtocolError;
use super::frame::Parser;
use super::proto::{CloseReason, OpCode};

/// An outgoing WebSocket message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// UTF-8 text message.
    Text(ByteString),
    /// Binary message.
    Binary(Bytes),
    /// Fragment of a text or binary message.
    Continuation(Item),
    /// Ping control message.
    Ping(Bytes),
    /// Pong control message.
    Pong(Bytes),
    /// Close control message with an optional reason.
    Close(Option<CloseReason>),
}

/// A decoded WebSocket frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Text frame.
    ///
    /// The codec does not validate that the payload is UTF-8.
    Text(Bytes),
    /// Binary frame.
    Binary(Bytes),
    /// Fragmented text or binary frame.
    Continuation(Item),
    /// Ping control frame.
    Ping(Bytes),
    /// Pong control frame.
    Pong(Bytes),
    /// Close control frame with an optional reason.
    Close(Option<CloseReason>),
}

/// A fragment in a WebSocket continuation sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// First fragment of a text message.
    FirstText(Bytes),
    /// First fragment of a binary message.
    FirstBinary(Bytes),
    /// Intermediate fragment.
    Continue(Bytes),
    /// Final fragment.
    Last(Bytes),
}

#[derive(Debug, Clone)]
/// Encoder and decoder for WebSocket frames.
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
        const R_CLOSED       = 0b0001_0000;
    }
}

impl Codec {
    /// Creates a codec in server mode with a 64 KiB frame-size limit.
    #[must_use]
    pub fn new() -> Codec {
        Codec {
            max_size: 65_536,
            flags: Cell::new(Flags::SERVER),
        }
    }

    /// Sets the maximum accepted frame payload size.
    ///
    /// The default is 64 KiB.
    #[must_use]
    pub fn max_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    /// Configures the codec for client-side masking rules.
    ///
    /// Client mode masks encoded frames and rejects masked incoming frames.
    /// By default, the codec uses server-side masking rules.
    #[must_use]
    pub fn set_client_mode(self) -> Self {
        self.remove_flags(Flags::SERVER);
        self
    }

    /// Returns `true` after this codec has encoded a close message.
    pub fn is_closed(&self) -> bool {
        self.flags().contains(Flags::CLOSED)
    }

    fn flags(&self) -> Flags {
        self.flags.get()
    }

    fn insert_flags(&self, f: Flags) {
        self.flags.set(self.flags() | f);
    }

    fn remove_flags(&self, f: Flags) {
        self.flags.set(self.flags() - f);
    }

    /// Encodes `page` as a final binary frame.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Closed`] if a close message has already been
    /// encoded, or [`ProtocolError::ContinuationStarted`] if a fragmented
    /// message is in progress.
    pub fn encode_page(&self, page: BytePage, dst: &mut BytePages) -> Result<(), ProtocolError> {
        if self.is_closed() {
            return Err(ProtocolError::Closed);
        }
        if self.flags().contains(Flags::W_CONTINUATION) {
            return Err(ProtocolError::ContinuationStarted);
        }
        Parser::write_message(
            dst,
            page,
            OpCode::Binary,
            true,
            !self.flags().contains(Flags::SERVER),
        )
        .expect("binary frames are always valid");
        Ok(())
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

    fn encode(&self, item: Message, dst: &mut BytePages) -> Result<(), Self::Error> {
        if self.is_closed() {
            return Err(ProtocolError::Closed);
        }

        match item {
            Message::Text(txt) => {
                if self.flags().contains(Flags::W_CONTINUATION) {
                    return Err(ProtocolError::ContinuationStarted);
                }
                Parser::write_message(
                    dst,
                    txt,
                    OpCode::Text,
                    true,
                    !self.flags().contains(Flags::SERVER),
                )?;
            }
            Message::Binary(bin) => {
                if self.flags().contains(Flags::W_CONTINUATION) {
                    return Err(ProtocolError::ContinuationStarted);
                }
                Parser::write_message(
                    dst,
                    bin,
                    OpCode::Binary,
                    true,
                    !self.flags().contains(Flags::SERVER),
                )?;
            }
            Message::Ping(txt) => Parser::write_message(
                dst,
                txt,
                OpCode::Ping,
                true,
                !self.flags().contains(Flags::SERVER),
            )?,
            Message::Pong(txt) => Parser::write_message(
                dst,
                txt,
                OpCode::Pong,
                true,
                !self.flags().contains(Flags::SERVER),
            )?,
            Message::Close(reason) => {
                Parser::write_close(dst, reason, !self.flags().contains(Flags::SERVER))?;
                self.insert_flags(Flags::CLOSED);
            }
            Message::Continuation(cont) => match cont {
                Item::FirstText(data) => {
                    if self.flags().contains(Flags::W_CONTINUATION) {
                        return Err(ProtocolError::ContinuationStarted);
                    }
                    self.insert_flags(Flags::W_CONTINUATION);
                    Parser::write_message(
                        dst,
                        data,
                        OpCode::Text,
                        false,
                        !self.flags().contains(Flags::SERVER),
                    )?;
                }
                Item::FirstBinary(data) => {
                    if self.flags().contains(Flags::W_CONTINUATION) {
                        return Err(ProtocolError::ContinuationStarted);
                    }
                    self.insert_flags(Flags::W_CONTINUATION);
                    Parser::write_message(
                        dst,
                        data,
                        OpCode::Binary,
                        false,
                        !self.flags().contains(Flags::SERVER),
                    )?;
                }
                Item::Continue(data) => {
                    if self.flags().contains(Flags::W_CONTINUATION) {
                        Parser::write_message(
                            dst,
                            data,
                            OpCode::Continue,
                            false,
                            !self.flags().contains(Flags::SERVER),
                        )?;
                    } else {
                        return Err(ProtocolError::ContinuationNotStarted);
                    }
                }
                Item::Last(data) => {
                    if self.flags().contains(Flags::W_CONTINUATION) {
                        self.remove_flags(Flags::W_CONTINUATION);
                        Parser::write_message(
                            dst,
                            data,
                            OpCode::Continue,
                            true,
                            !self.flags().contains(Flags::SERVER),
                        )?;
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
        // the peer must not send anything after its close frame, discard it
        if self.flags().contains(Flags::R_CLOSED) {
            src.clear();
            return Ok(None);
        }

        match Parser::parse(src, self.flags().contains(Flags::SERVER), self.max_size) {
            Ok(Some((finished, opcode, payload))) => {
                // handle continuation
                if finished {
                    match opcode {
                        OpCode::Continue => {
                            if self.flags().contains(Flags::R_CONTINUATION) {
                                self.remove_flags(Flags::R_CONTINUATION);
                                let payload = payload.unwrap_or_default();
                                Ok(Some(Frame::Continuation(Item::Last(payload))))
                            } else {
                                Err(ProtocolError::ContinuationNotStarted)
                            }
                        }
                        OpCode::Close => {
                            let reason = if let Some(pl) = payload {
                                Parser::parse_close_payload(&pl)?
                            } else {
                                None
                            };
                            self.insert_flags(Flags::R_CLOSED);
                            Ok(Some(Frame::Close(reason)))
                        }
                        OpCode::Ping => Ok(Some(Frame::Ping(payload.unwrap_or_default()))),
                        OpCode::Pong => Ok(Some(Frame::Pong(payload.unwrap_or_default()))),
                        OpCode::Binary => {
                            if self.flags().contains(Flags::R_CONTINUATION) {
                                Err(ProtocolError::ContinuationStarted)
                            } else {
                                Ok(Some(Frame::Binary(payload.unwrap_or_else(Bytes::new))))
                            }
                        }
                        OpCode::Text => {
                            if self.flags().contains(Flags::R_CONTINUATION) {
                                Err(ProtocolError::ContinuationStarted)
                            } else {
                                Ok(Some(Frame::Text(payload.unwrap_or_else(Bytes::new))))
                            }
                        }
                    }
                } else {
                    match opcode {
                        OpCode::Continue => {
                            if self.flags().contains(Flags::R_CONTINUATION) {
                                Ok(Some(Frame::Continuation(Item::Continue(
                                    payload.unwrap_or_else(Bytes::new),
                                ))))
                            } else {
                                Err(ProtocolError::ContinuationNotStarted)
                            }
                        }
                        OpCode::Binary => {
                            if self.flags().contains(Flags::R_CONTINUATION) {
                                Err(ProtocolError::ContinuationStarted)
                            } else {
                                self.insert_flags(Flags::R_CONTINUATION);
                                Ok(Some(Frame::Continuation(Item::FirstBinary(
                                    payload.unwrap_or_else(Bytes::new),
                                ))))
                            }
                        }
                        OpCode::Text => {
                            if self.flags().contains(Flags::R_CONTINUATION) {
                                Err(ProtocolError::ContinuationStarted)
                            } else {
                                self.insert_flags(Flags::R_CONTINUATION);
                                Ok(Some(Frame::Continuation(Item::FirstText(
                                    payload.unwrap_or_else(Bytes::new),
                                ))))
                            }
                        }
                        // rejected by the parser, kept for exhaustiveness
                        OpCode::Ping | OpCode::Pong | OpCode::Close => {
                            Err(ProtocolError::FragmentedControlFrame(opcode))
                        }
                    }
                }
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::CloseCode;

    #[test]
    fn text_payload_is_not_validated() {
        let codec = Codec::new().set_client_mode();
        let mut frame = BytesMut::from(&[0x81, 0x01, 0xff][..]);
        assert!(matches!(
            codec.decode(&mut frame),
            Ok(Some(Frame::Text(data))) if data == Bytes::from_static(&[0xff])
        ));
    }

    #[test]
    fn input_after_close_is_discarded() {
        let codec = Codec::new().set_client_mode();
        // close frame, followed by a text frame
        let mut src = BytesMut::from(&[0x88, 0x00, 0x81, 0x01, b'a'][..]);
        assert!(matches!(
            codec.decode(&mut src),
            Ok(Some(Frame::Close(None)))
        ));
        assert!(matches!(codec.decode(&mut src), Ok(None)));
        assert!(src.is_empty());

        src.extend_from_slice(&[0x81, 0x01, b'b']);
        assert!(matches!(codec.decode(&mut src), Ok(None)));
        assert!(src.is_empty());
    }

    #[test]
    fn accepts_extension_close_code() {
        // servers should not send 1010, but receivers accept it
        let codec = Codec::new().set_client_mode();
        let mut close = BytesMut::from(&[0x88, 0x02, 0x03, 0xf2][..]);
        assert!(matches!(
            codec.decode(&mut close),
            Ok(Some(Frame::Close(Some(CloseReason {
                code: CloseCode::Extension,
                ..
            }))))
        ));

        // servers still cannot send it
        let codec = Codec::new();
        let mut dst = BytePages::default();
        assert!(matches!(
            codec.encode(Message::Close(Some(CloseCode::Extension.into())), &mut dst),
            Err(ProtocolError::InvalidCloseCode(1010))
        ));
    }

    #[test]
    fn validates_outgoing_continuations() {
        let codec = Codec::new();
        let mut dst = BytePages::default();
        codec
            .encode(
                Message::Continuation(Item::FirstBinary(Bytes::new())),
                &mut dst,
            )
            .unwrap();
        assert!(matches!(
            codec.encode(Message::Text("text".into()), &mut dst),
            Err(ProtocolError::ContinuationStarted)
        ));
        assert!(matches!(
            codec.encode_page(BytePage::from(Bytes::new()), &mut dst),
            Err(ProtocolError::ContinuationStarted)
        ));

        codec
            .encode(Message::Continuation(Item::Last(Bytes::new())), &mut dst)
            .unwrap();
        codec
            .encode_page(BytePage::from(Bytes::new()), &mut dst)
            .unwrap();
    }

    #[test]
    fn rejects_messages_after_close() {
        let codec = Codec::new();
        let mut dst = BytePages::default();
        codec.encode(Message::Close(None), &mut dst).unwrap();

        assert!(matches!(
            codec.encode(Message::Text("text".into()), &mut dst),
            Err(ProtocolError::Closed)
        ));
        assert!(matches!(
            codec.encode_page(BytePage::from(Bytes::new()), &mut dst),
            Err(ProtocolError::Closed)
        ));
    }
}
