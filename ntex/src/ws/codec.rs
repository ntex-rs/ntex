use std::cell::Cell;
#[cfg(feature = "compress")]
use std::rc::Rc;

use crate::codec::{Decoder, Encoder};
use crate::util::{BytePage, BytePages, ByteString, Bytes, BytesMut};

#[cfg(feature = "compress")]
use super::deflate::{Deflate, DeflateConfig};
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
    /// The payload is valid UTF-8 unless validation is disabled with
    /// [`Codec::validate_utf8()`].
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
///
/// When decoding, a single fragment of a text message is not necessarily
/// valid UTF-8 on its own, a character can be split between fragments. The
/// codec validates the text of the whole message, see
/// [`Codec::validate_utf8()`].
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
///
/// With the `compress` feature, the codec can use the `permessage-deflate`
/// extension, see [`Codec::set_deflate()`]. Clones of a codec share the
/// compression state.
///
/// The decoder limits the payload of each frame, see [`Codec::max_size()`],
/// and the total payload of a message, see [`Codec::max_message_size()`].
/// It also checks that text messages are valid UTF-8, see
/// [`Codec::validate_utf8()`].
pub struct Codec {
    flags: Cell<Flags>,
    max_size: usize,
    max_message_size: usize,
    validate_utf8: bool,
    /// Payload size of the incoming fragmented message.
    msg_size: Cell<usize>,
    /// Incomplete UTF-8 character at the end of the last text fragment.
    utf8_tail: Cell<Utf8Tail>,
    #[cfg(feature = "compress")]
    deflate: Option<Rc<Deflate>>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const SERVER         = 0b0000_0001;
        const R_CONTINUATION = 0b0000_0010;
        const W_CONTINUATION = 0b0000_0100;
        const CLOSED         = 0b0000_1000;
        const R_CLOSED       = 0b0001_0000;
        const R_COMPRESSED   = 0b0010_0000;
        const R_TEXT         = 0b0100_0000;
    }
}

#[derive(Copy, Clone, Debug, Default)]
struct Utf8Tail {
    buf: [u8; 3],
    len: u8,
}

/// Validates the next fragment of a UTF-8 text message.
///
/// `tail` is the incomplete character at the end of the previous fragment,
/// the result is the incomplete character at the end of `data`. A message
/// must not end with an incomplete character.
fn validate_utf8(tail: Utf8Tail, mut data: &[u8], fin: bool) -> Result<Utf8Tail, ProtocolError> {
    if tail.len > 0 {
        // complete the character started by the previous fragment
        let mut ch = [0; 4];
        let mut len = usize::from(tail.len);
        ch[..len].copy_from_slice(&tail.buf[..len]);
        loop {
            let Some((&b, rest)) = data.split_first() else {
                return if fin {
                    Err(ProtocolError::InvalidUtf8)
                } else {
                    Ok(Utf8Tail {
                        buf: [ch[0], ch[1], ch[2]],
                        len: len as u8,
                    })
                };
            };
            // a prefix of three bytes is the longest incomplete character
            ch[len] = b;
            len += 1;
            data = rest;
            match simdutf8::compat::from_utf8(&ch[..len]) {
                Ok(_) => break,
                Err(e) if e.error_len().is_some() => return Err(ProtocolError::InvalidUtf8),
                Err(_) => {}
            }
        }
    }

    if simdutf8::basic::from_utf8(data).is_ok() {
        return Ok(Utf8Tail::default());
    }
    match simdutf8::compat::from_utf8(data) {
        Err(e) if e.error_len().is_none() && !fin => {
            let rest = &data[e.valid_up_to()..];
            let mut tail = Utf8Tail {
                len: rest.len() as u8,
                ..Utf8Tail::default()
            };
            tail.buf[..rest.len()].copy_from_slice(rest);
            Ok(tail)
        }
        _ => Err(ProtocolError::InvalidUtf8),
    }
}

impl Codec {
    /// Creates a codec in server mode.
    ///
    /// The codec limits frames to 64 KiB and messages to 4 MiB of payload,
    /// and validates incoming text messages.
    #[must_use]
    pub fn new() -> Codec {
        Codec {
            max_size: 65_536,
            max_message_size: 4 * 1024 * 1024,
            validate_utf8: true,
            msg_size: Cell::new(0),
            utf8_tail: Cell::new(Utf8Tail::default()),
            flags: Cell::new(Flags::SERVER),
            #[cfg(feature = "compress")]
            deflate: None,
        }
    }

    /// Sets the maximum accepted frame payload size.
    ///
    /// The default is 64 KiB. With the `permessage-deflate` extension, the
    /// limit applies to both the compressed and the decompressed payload of
    /// a frame.
    ///
    /// A fragmented message can consist of many frames, its total size is
    /// limited by [`Codec::max_message_size()`].
    #[must_use]
    pub fn max_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    /// Sets the maximum accepted payload size of an incoming message.
    ///
    /// The limit applies to the total payload of all frames of a fragmented
    /// message, and to single-frame messages. With the `permessage-deflate`
    /// extension, it applies to the decompressed payload. Control frames are
    /// not counted. A message that exceeds the limit fails with
    /// [`ProtocolError::Overflow`].
    ///
    /// The default is 4 MiB. Raise it together with [`Codec::max_size()`]
    /// to accept larger single-frame messages, use `usize::MAX` to only
    /// limit frames, for example to stream data with
    /// [`WsTransport`](super::WsTransport).
    #[must_use]
    pub fn max_message_size(mut self, size: usize) -> Self {
        self.max_message_size = size;
        self
    }

    /// Enables or disables UTF-8 validation of incoming text messages.
    ///
    /// When enabled, the payload of [`Frame::Text`] is valid UTF-8 and the
    /// fragments of a text message form valid UTF-8 when joined. A single
    /// fragment can end or start in the middle of a character. Invalid text
    /// fails with [`ProtocolError::InvalidUtf8`] as soon as it is detected,
    /// as RFC 6455 requires.
    ///
    /// Validation is enabled by default.
    #[must_use]
    pub fn validate_utf8(mut self, enabled: bool) -> Self {
        self.validate_utf8 = enabled;
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

    #[cfg(feature = "compress")]
    /// Enables the `permessage-deflate` extension with the agreed parameters.
    ///
    /// Outgoing text and binary messages are compressed, incoming messages
    /// with the RSV1 bit set are decompressed. The parameters must be the
    /// result of the extension negotiation, the client and server roles of
    /// the parameters follow the codec mode.
    #[must_use]
    pub fn set_deflate(mut self, cfg: DeflateConfig) -> Self {
        self.deflate = Some(Rc::new(Deflate::new(cfg)));
        self
    }

    #[cfg(feature = "compress")]
    /// Returns the `permessage-deflate` parameters, if the extension is used.
    pub fn deflate(&self) -> Option<&DeflateConfig> {
        self.deflate.as_deref().map(Deflate::config)
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

    fn is_server(&self) -> bool {
        self.flags().contains(Flags::SERVER)
    }

    #[cfg_attr(not(feature = "compress"), allow(clippy::unused_self))]
    fn is_compressed(&self) -> bool {
        #[cfg(feature = "compress")]
        {
            self.deflate.is_some()
        }
        #[cfg(not(feature = "compress"))]
        {
            false
        }
    }

    /// Writes a data frame, compressed if the extension is used.
    ///
    /// `first` marks the first frame of a message, it carries the RSV1 bit.
    fn write_data<B>(
        &self,
        dst: &mut BytePages,
        payload: B,
        op: OpCode,
        first: bool,
        fin: bool,
    ) -> Result<(), ProtocolError>
    where
        BytePage: From<B>,
    {
        let payload = BytePage::from(payload);
        #[cfg(feature = "compress")]
        if let Some(deflate) = &self.deflate
            && !(first && fin && deflate.is_skipped(payload.len(), self.is_server()))
        {
            let payload = deflate.compress(payload.as_ref(), fin, self.is_server());
            return Parser::write_frame(dst, payload.into(), op, fin, first, !self.is_server());
        }
        let _ = first;
        Parser::write_frame(dst, payload, op, fin, false, !self.is_server())
    }

    /// Decompresses the payload of a data frame, if the message is
    /// compressed.
    #[cfg_attr(
        not(feature = "compress"),
        allow(clippy::unused_self, clippy::unnecessary_wraps)
    )]
    #[inline]
    fn read_data(&self, payload: Option<Bytes>, fin: bool) -> Result<Bytes, ProtocolError> {
        // only set if the extension is used
        #[cfg(feature = "compress")]
        if self.flags().contains(Flags::R_COMPRESSED) {
            return self.decompress(payload, fin);
        }
        let _ = fin;
        Ok(payload.unwrap_or_default())
    }

    #[cfg(feature = "compress")]
    #[inline(never)]
    fn decompress(&self, payload: Option<Bytes>, fin: bool) -> Result<Bytes, ProtocolError> {
        let payload = payload.unwrap_or_default();
        if fin {
            self.remove_flags(Flags::R_COMPRESSED);
        }
        self.deflate
            .as_ref()
            .expect("compressed messages require the extension")
            .decompress(&payload, fin, self.is_server(), self.max_size)
            .map(BytesMut::freeze)
    }

    /// Checks the payload of an incoming data frame against the message size
    /// limit and, for a text message, validates its UTF-8 encoding.
    ///
    /// `first` is `Some(is_text)` for the first frame of a message.
    fn check_data(
        &self,
        data: Bytes,
        first: Option<bool>,
        fin: bool,
    ) -> Result<Bytes, ProtocolError> {
        let (size, text) = match first {
            Some(text) => (data.len(), text),
            None => (
                self.msg_size.get().saturating_add(data.len()),
                self.flags().contains(Flags::R_TEXT),
            ),
        };
        if size > self.max_message_size {
            return Err(ProtocolError::Overflow);
        }
        if text && self.validate_utf8 {
            let tail = if first.is_some() {
                Utf8Tail::default()
            } else {
                self.utf8_tail.get()
            };
            self.utf8_tail.set(validate_utf8(tail, &data, fin)?);
        }

        if !fin {
            self.msg_size.set(size);
            if text {
                self.insert_flags(Flags::R_TEXT);
            } else {
                self.remove_flags(Flags::R_TEXT);
            }
        }
        Ok(data)
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
        self.write_data(dst, page, OpCode::Binary, true, true)
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
                self.write_data(dst, txt, OpCode::Text, true, true)?;
            }
            Message::Binary(bin) => {
                if self.flags().contains(Flags::W_CONTINUATION) {
                    return Err(ProtocolError::ContinuationStarted);
                }
                self.write_data(dst, bin, OpCode::Binary, true, true)?;
            }
            Message::Ping(txt) => {
                Parser::write_message(dst, txt, OpCode::Ping, true, !self.is_server())?;
            }
            Message::Pong(txt) => {
                Parser::write_message(dst, txt, OpCode::Pong, true, !self.is_server())?;
            }
            Message::Close(reason) => {
                Parser::write_close(dst, reason, !self.is_server())?;
                self.insert_flags(Flags::CLOSED);
            }
            Message::Continuation(cont) => match cont {
                Item::FirstText(data) => {
                    if self.flags().contains(Flags::W_CONTINUATION) {
                        return Err(ProtocolError::ContinuationStarted);
                    }
                    self.insert_flags(Flags::W_CONTINUATION);
                    self.write_data(dst, data, OpCode::Text, true, false)?;
                }
                Item::FirstBinary(data) => {
                    if self.flags().contains(Flags::W_CONTINUATION) {
                        return Err(ProtocolError::ContinuationStarted);
                    }
                    self.insert_flags(Flags::W_CONTINUATION);
                    self.write_data(dst, data, OpCode::Binary, true, false)?;
                }
                Item::Continue(data) => {
                    if self.flags().contains(Flags::W_CONTINUATION) {
                        self.write_data(dst, data, OpCode::Continue, false, false)?;
                    } else {
                        return Err(ProtocolError::ContinuationNotStarted);
                    }
                }
                Item::Last(data) => {
                    if self.flags().contains(Flags::W_CONTINUATION) {
                        self.remove_flags(Flags::W_CONTINUATION);
                        self.write_data(dst, data, OpCode::Continue, false, true)?;
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

        let Some((finished, rsv1, opcode, payload)) =
            Parser::parse_frame(src, self.is_server(), self.max_size, self.is_compressed())?
        else {
            return Ok(None);
        };

        // only the first frame of a data message can be compressed
        if rsv1 {
            if !matches!(opcode, OpCode::Text | OpCode::Binary) {
                return Err(ProtocolError::ReservedBits(0b100));
            }
            if !self.flags().contains(Flags::R_CONTINUATION) {
                self.insert_flags(Flags::R_COMPRESSED);
            }
        }

        // handle continuation
        if finished {
            match opcode {
                OpCode::Continue => {
                    if self.flags().contains(Flags::R_CONTINUATION) {
                        self.remove_flags(Flags::R_CONTINUATION);
                        let data = self.read_data(payload, true)?;
                        Ok(Some(Frame::Continuation(Item::Last(
                            self.check_data(data, None, true)?,
                        ))))
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
                        let data = self.read_data(payload, true)?;
                        Ok(Some(Frame::Binary(self.check_data(
                            data,
                            Some(false),
                            true,
                        )?)))
                    }
                }
                OpCode::Text => {
                    if self.flags().contains(Flags::R_CONTINUATION) {
                        Err(ProtocolError::ContinuationStarted)
                    } else {
                        let data = self.read_data(payload, true)?;
                        Ok(Some(Frame::Text(self.check_data(
                            data,
                            Some(true),
                            true,
                        )?)))
                    }
                }
            }
        } else {
            match opcode {
                OpCode::Continue => {
                    if self.flags().contains(Flags::R_CONTINUATION) {
                        let data = self.read_data(payload, false)?;
                        Ok(Some(Frame::Continuation(Item::Continue(
                            self.check_data(data, None, false)?,
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
                        let data = self.read_data(payload, false)?;
                        Ok(Some(Frame::Continuation(Item::FirstBinary(
                            self.check_data(data, Some(false), false)?,
                        ))))
                    }
                }
                OpCode::Text => {
                    if self.flags().contains(Flags::R_CONTINUATION) {
                        Err(ProtocolError::ContinuationStarted)
                    } else {
                        self.insert_flags(Flags::R_CONTINUATION);
                        let data = self.read_data(payload, false)?;
                        Ok(Some(Frame::Continuation(Item::FirstText(
                            self.check_data(data, Some(true), false)?,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::CloseCode;

    /// STATEMENT: Text frames are UTF-8 validated by default, and disabling validation returns the payload unchanged.
    ///
    /// HOW:
    /// - Decode a text frame containing `0xff` with a default codec and expect `InvalidUtf8`.
    /// - Expect `InvalidUtf8` for a text frame that ends inside a multi-byte character.
    /// - Decode valid multi-byte text with a default codec and expect it unchanged.
    /// - Decode the invalid frame with `validate_utf8(false)` and expect the raw byte back.
    #[test]
    fn text_payload_is_validated() {
        let codec = Codec::new().set_client_mode();
        assert!(matches!(
            decode(&codec, &[0x81, 0x01, 0xff]),
            Err(ProtocolError::InvalidUtf8)
        ));
        assert!(matches!(
            decode(&Codec::new().set_client_mode(), b"\x81\x02a\xe2"),
            Err(ProtocolError::InvalidUtf8)
        ));
        assert!(matches!(
            decode(&Codec::new().set_client_mode(), b"\x81\x03a\xc3\xa9"),
            Ok(Some(Frame::Text(data))) if data == "a\u{e9}".as_bytes()
        ));

        let codec = Codec::new().set_client_mode().validate_utf8(false);
        assert!(matches!(
            decode(&codec, &[0x81, 0x01, 0xff]),
            Ok(Some(Frame::Text(data))) if data == Bytes::from_static(&[0xff])
        ));
    }

    /// STATEMENT: Fragmented text is validated as a whole message, so characters may span fragments but the message must be valid UTF-8.
    ///
    /// HOW:
    /// - Split `\u{20ac}` (`e2 82 ac`) over first, continue, and last fragments with an empty fragment in between and expect all to decode.
    /// - Expect `InvalidUtf8` when a split character is completed with a non-continuation byte, when a message ends inside a character, and for an invalid byte in a later fragment.
    /// - Decode binary fragments with `0xff` and confirm binary data is not validated.
    #[test]
    fn fragmented_text_is_validated() {
        let codec = Codec::new().set_client_mode();
        for frame in [
            &b"\x01\x02a\xe2"[..],
            b"\x00\x00",
            b"\x00\x01\x82",
            b"\x80\x02\xacb",
        ] {
            assert!(matches!(
                decode(&codec, frame),
                Ok(Some(Frame::Continuation(_)))
            ));
        }

        let codec = Codec::new().set_client_mode();
        assert!(decode(&codec, b"\x01\x01\xe2").is_ok());
        assert!(matches!(
            decode(&codec, b"\x80\x01a"),
            Err(ProtocolError::InvalidUtf8)
        ));

        let codec = Codec::new().set_client_mode();
        assert!(decode(&codec, b"\x01\x02a\xe2").is_ok());
        assert!(decode(&codec, b"\x00\x01\x82").is_ok());
        assert!(matches!(
            decode(&codec, b"\x80\x00"),
            Err(ProtocolError::InvalidUtf8)
        ));

        let codec = Codec::new().set_client_mode();
        assert!(decode(&codec, b"\x01\x01a").is_ok());
        assert!(matches!(
            decode(&codec, b"\x00\x02b\xff"),
            Err(ProtocolError::InvalidUtf8)
        ));

        let codec = Codec::new().set_client_mode();
        assert!(decode(&codec, b"\x02\x01\xe2").is_ok());
        assert!(matches!(
            decode(&codec, b"\x80\x01\xff"),
            Ok(Some(Frame::Continuation(Item::Last(_))))
        ));
    }

    /// STATEMENT: The message size limit bounds the total payload of a fragmented message, applies to single frames, ignores control frames, and resets per message.
    ///
    /// HOW:
    /// - With `max_message_size(3)`, decode a 3-byte message split by a ping and expect success, then a second message of the same size.
    /// - Expect `Overflow` for a fragment that takes the total to 4 bytes and for a 4-byte single frame.
    #[test]
    fn message_size_limit() {
        let codec = Codec::new().set_client_mode().max_message_size(3);
        for _ in 0..2 {
            assert!(decode(&codec, b"\x02\x02ab").is_ok());
            assert!(matches!(
                decode(&codec, b"\x89\x04ping"),
                Ok(Some(Frame::Ping(_)))
            ));
            assert!(matches!(
                decode(&codec, b"\x80\x01c"),
                Ok(Some(Frame::Continuation(Item::Last(_))))
            ));
        }

        assert!(decode(&codec, b"\x01\x02ab").is_ok());
        assert!(decode(&codec, b"\x00\x01c").is_ok());
        assert!(matches!(
            decode(&codec, b"\x80\x01d"),
            Err(ProtocolError::Overflow)
        ));

        let codec = Codec::new().set_client_mode().max_message_size(3);
        assert!(matches!(
            decode(&codec, b"\x82\x04abcd"),
            Err(ProtocolError::Overflow)
        ));
    }

    /// STATEMENT: Receiving close discards both buffered and subsequently received frames.
    ///
    /// HOW:
    /// - Decode close followed by text, then add another text frame and verify both later decodes clear input without output.
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

    /// STATEMENT: Receivers accept extension close code 1010 even though server-side encoders reject sending it.
    ///
    /// HOW:
    /// - Decode a peer close with code 1010 and attempt to encode the same reason in server mode.
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

    /// STATEMENT: An active fragmented message prevents unrelated text or page messages until its last fragment.
    ///
    /// HOW:
    /// - Start a binary continuation, reject text/page sends, end it, and verify a page can then be encoded.
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

    /// STATEMENT: Sending close prevents later text and page messages.
    ///
    /// HOW:
    /// - Encode close, then check both outgoing data APIs return the closed-protocol error.
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

    /// STATEMENT: Encoding rejects oversized controls and invalid continuation transitions.
    ///
    /// HOW:
    /// - Reject oversized ping/pong, new data during a text continuation, and continuation after its last fragment.
    #[test]
    fn encode_errors() {
        let codec = Codec::new();
        let mut dst = BytePages::default();
        let big = Bytes::from(vec![0; 126]);
        assert!(matches!(
            codec.encode(Message::Ping(big.clone()), &mut dst),
            Err(ProtocolError::InvalidLength(126))
        ));
        assert!(matches!(
            codec.encode(Message::Pong(big), &mut dst),
            Err(ProtocolError::InvalidLength(126))
        ));

        codec
            .encode(Message::Continuation(Item::FirstText("a".into())), &mut dst)
            .unwrap();
        assert!(matches!(
            codec.encode(Message::Binary("b".into()), &mut dst),
            Err(ProtocolError::ContinuationStarted)
        ));
        assert!(matches!(
            codec.encode(
                Message::Continuation(Item::FirstBinary("b".into())),
                &mut dst
            ),
            Err(ProtocolError::ContinuationStarted)
        ));
        codec
            .encode(Message::Continuation(Item::Last("c".into())), &mut dst)
            .unwrap();
        assert!(matches!(
            codec.encode(Message::Continuation(Item::Continue("d".into())), &mut dst),
            Err(ProtocolError::ContinuationNotStarted)
        ));
    }

    fn decode(codec: &Codec, frame: &[u8]) -> Result<Option<Frame>, ProtocolError> {
        codec.decode(&mut BytesMut::from(frame))
    }

    /// STATEMENT: Decoding enforces fragmented-message ordering without preventing valid later continuation frames.
    ///
    /// HOW:
    /// - Reject orphan continuations and new data while a binary message is open.
    /// - Continue that message and verify an empty final fragment completes it.
    #[test]
    fn decode_continuation_errors() {
        let codec = Codec::new().set_client_mode();
        // continuation without a first frame
        assert!(matches!(
            decode(&codec, &[0x80, 0x01, b'a']),
            Err(ProtocolError::ContinuationNotStarted)
        ));
        assert!(matches!(
            decode(&codec, &[0x00, 0x01, b'a']),
            Err(ProtocolError::ContinuationNotStarted)
        ));

        // new data frames while a fragmented message is in progress
        assert!(matches!(
            decode(&codec, &[0x02, 0x01, b'a']),
            Ok(Some(Frame::Continuation(Item::FirstBinary(_))))
        ));
        for frame in [
            &[0x82, 0x01, b'a'],
            &[0x81, 0x01, b'a'],
            &[0x02, 0x01, b'a'],
            &[0x01, 0x01, b'a'],
        ] {
            assert!(matches!(
                decode(&codec, frame),
                Err(ProtocolError::ContinuationStarted)
            ));
        }
        assert!(matches!(
            decode(&codec, &[0x00, 0x01, b'b']),
            Ok(Some(Frame::Continuation(Item::Continue(_))))
        ));
        assert!(matches!(
            decode(&codec, &[0x80, 0x00]),
            Ok(Some(Frame::Continuation(Item::Last(data)))) if data.is_empty()
        ));
    }

    #[cfg(feature = "compress")]
    mod deflate {
        use super::*;
        use crate::ws::DeflateConfig;

        fn client() -> Codec {
            Codec::new()
                .set_client_mode()
                .set_deflate(DeflateConfig::new())
        }

        fn server() -> Codec {
            Codec::new().set_deflate(DeflateConfig::new())
        }

        fn encode(codec: &Codec, msg: Message) -> BytesMut {
            let mut dst = BytePages::default();
            codec.encode(msg, &mut dst).unwrap();
            BytesMut::from(&Bytes::from(dst)[..])
        }

        fn decode_all(codec: &Codec, mut src: BytesMut) -> Vec<Frame> {
            let mut frames = Vec::new();
            while let Some(frame) = codec.decode(&mut src).unwrap() {
                frames.push(frame);
            }
            assert!(src.is_empty());
            frames
        }

        fn text(data: &'static str) -> Frame {
            Frame::Text(Bytes::from_static(data.as_bytes()))
        }

        /// STATEMENT: Deflate frame decoding matches RFC 7692 examples across fragmentation, context reuse, and block forms.
        ///
        /// HOW:
        /// - Decode published single/fragmented, context-dependent, stored, final-block, multi-block, and empty messages.
        /// - Compare their text and continuation outputs.
        #[test]
        fn rfc7692_examples() {
            // 7.2.3.1, a message in one frame and in two fragments
            let frames = decode_all(
                &client(),
                BytesMut::from(&[0xc1, 0x07, 0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00][..]),
            );
            assert_eq!(frames, [text("Hello")]);
            let frames = decode_all(
                &client(),
                BytesMut::from(
                    &[
                        0x41, 0x03, 0xf2, 0x48, 0xcd, 0x80, 0x04, 0xc9, 0xc9, 0x07, 0x00,
                    ][..],
                ),
            );
            assert_eq!(
                frames,
                [
                    Frame::Continuation(Item::FirstText(Bytes::from_static(b"He"))),
                    Frame::Continuation(Item::Last(Bytes::from_static(b"llo"))),
                ]
            );

            // 7.2.3.2, the second message refers to the first one
            let frames = decode_all(
                &client(),
                BytesMut::from(
                    &[
                        0xc1, 0x07, 0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0xc1, 0x05, 0xf2,
                        0x00, 0x11, 0x00, 0x00,
                    ][..],
                ),
            );
            assert_eq!(frames, [text("Hello"), text("Hello")]);

            // 7.2.3.3, a stored block
            // 7.2.3.4, a block with BFINAL set, followed by a new stream
            // 7.2.3.5, two blocks in one message
            // 7.2.3.6, an empty message
            let frames = decode_all(
                &client(),
                BytesMut::from(
                    &[
                        0xc1, 0x0b, 0x00, 0x05, 0x00, 0xfa, 0xff, 0x48, 0x65, 0x6c, 0x6c, 0x6f,
                        0x00, 0xc1, 0x08, 0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x00, 0xc1,
                        0x07, 0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0xc1, 0x0d, 0xf2, 0x48,
                        0x05, 0x00, 0x00, 0x00, 0xff, 0xff, 0xca, 0xc9, 0xc9, 0x07, 0x00, 0xc1,
                        0x01, 0x00, 0xc1, 0x00,
                    ][..],
                ),
            );
            assert_eq!(
                frames,
                [
                    text("Hello"),
                    text("Hello"),
                    text("Hello"),
                    text("Hello"),
                    text(""),
                    text("")
                ]
            );
        }

        /// STATEMENT: No-context-takeover servers skip small whole messages but still compress fragments.
        ///
        /// HOW:
        /// - Inspect frame flags and round trips around the small-message threshold, then encode a fragment.
        /// - Verify the client still compresses small messages when its context remains enabled.
        #[test]
        fn small_messages_without_context_takeover() {
            let cfg = DeflateConfig::new().set_server_no_context_takeover(true);
            let server = Codec::new().set_deflate(cfg.clone());
            let client = Codec::new().set_client_mode().set_deflate(cfg);

            // small unfragmented messages are not compressed
            for (len, first) in [(0, 0x82), (32, 0x82), (33, 0xc2)] {
                let data = Bytes::from("a".repeat(len));
                let frame = encode(&server, Message::Binary(data.clone()));
                assert_eq!(frame[0], first);
                assert_eq!(decode_all(&client, frame), [Frame::Binary(data)]);
            }

            // fragmented messages are compressed
            let frame = encode(
                &server,
                Message::Continuation(Item::FirstText(Bytes::from_static(b"a"))),
            );
            assert_eq!(frame[0], 0x41);

            // the client keeps its context
            let frame = encode(&client, Message::Text("Hello".into()));
            assert_eq!(frame[0], 0xc1);
        }

        /// STATEMENT: Deflate text encoding reproduces the RFC 7692 Hello example.
        ///
        /// HOW:
        /// - Encode the example message and compare every compressed frame byte.
        #[test]
        fn encode_rfc7692_example() {
            let codec = Codec::new().set_deflate(DeflateConfig::new());
            assert_eq!(
                &encode(&codec, Message::Text("Hello".into()))[..],
                &[0xc1, 0x07, 0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00]
            );
        }

        /// STATEMENT: Deflate codecs round-trip mixed messages and interleaved controls in both client/server directions.
        ///
        /// HOW:
        /// - Repeatedly encode text, large/empty binary data, controls, and text/binary continuations.
        /// - Decode each combined buffer and compare the complete frame sequence.
        #[test]
        fn roundtrip() {
            let (client, server) = (client(), server());
            let data: Vec<u8> = (0..60_000u32)
                .map(|i| if i % 7 == 0 { (i >> 3) as u8 } else { b'a' })
                .collect();
            let data = Bytes::from(data);

            for _ in 0..3 {
                for (from, to) in [(&client, &server), (&server, &client)] {
                    let msgs = [
                        Message::Text("text message".into()),
                        Message::Binary(data.clone()),
                        Message::Binary(Bytes::new()),
                        Message::Ping("ping".into()),
                        Message::Continuation(Item::FirstBinary(data.slice(..1000))),
                        Message::Pong("pong".into()),
                        Message::Continuation(Item::Continue(Bytes::new())),
                        Message::Continuation(Item::Continue(data.slice(1000..30_000))),
                        Message::Continuation(Item::Last(data.slice(30_000..))),
                        Message::Continuation(Item::FirstText("a".into())),
                        Message::Continuation(Item::Last("b".into())),
                    ];
                    let mut src = BytesMut::new();
                    for msg in msgs {
                        src.extend_from_slice(&encode(from, msg));
                    }
                    assert_eq!(
                        decode_all(to, src),
                        [
                            text("text message"),
                            Frame::Binary(data.clone()),
                            Frame::Binary(Bytes::new()),
                            Frame::Ping("ping".into()),
                            Frame::Continuation(Item::FirstBinary(data.slice(..1000))),
                            Frame::Pong("pong".into()),
                            Frame::Continuation(Item::Continue(Bytes::new())),
                            Frame::Continuation(Item::Continue(data.slice(1000..30_000))),
                            Frame::Continuation(Item::Last(data.slice(30_000..))),
                            Frame::Continuation(Item::FirstText("a".into())),
                            Frame::Continuation(Item::Last("b".into())),
                        ]
                    );
                }
            }
        }

        /// STATEMENT: Compressed payloads remain decodable when reframed into small fragments with an empty final fragment.
        ///
        /// HOW:
        /// - Split compressed binary messages into 16-byte frames, append an empty last frame, and repeat with reused contexts.
        /// - Reassemble decoded continuation data and compare the original payload.
        #[test]
        fn fragmented_compressed_payload() {
            // a compressed message split into frames of 16 bytes, like the
            // auto-fragmentation of autobahn, the last frame can be empty
            let sender = server();
            let codec = client();
            let data = Bytes::from(b"Hello, Hello, Hello".repeat(40));
            for _ in 0..3 {
                let mut frame = encode(&sender, Message::Binary(data.clone()));
                frame.advance_to(2);
                let len = frame.len();
                frame.advance_to(if len < 126 { 0 } else { 2 });
                let payload = frame.freeze();
                let chunks: Vec<_> = payload.chunks(16).chain([&[][..]]).collect();

                let mut src = BytesMut::new();
                for (i, chunk) in chunks.iter().enumerate() {
                    let mut first = if i == 0 { 0x42 } else { 0x00 };
                    if i == chunks.len() - 1 {
                        first |= 0x80;
                    }
                    src.extend_from_slice(&[first, chunk.len() as u8]);
                    src.extend_from_slice(chunk);
                }
                let mut out = BytesMut::new();
                for frame in decode_all(&codec, src) {
                    match frame {
                        Frame::Continuation(
                            Item::FirstBinary(b) | Item::Continue(b) | Item::Last(b),
                        ) => out.extend_from_slice(&b),
                        f => panic!("{f:?}"),
                    }
                }
                assert_eq!(out, data);
            }
        }

        /// STATEMENT: Deflate sets RSV1 only on initial data frames and leaves controls uncompressed.
        ///
        /// HOW:
        /// - Inspect whole-message, continuation, ping, and close wire flags and bytes.
        /// - Encode a binary page and verify its compressed flag and decoded payload.
        #[test]
        fn rsv1_only_on_first_data_frame() {
            let codec = server();
            let data = Bytes::from(vec![b'x'; 1000]);
            let frame = encode(&codec, Message::Binary(data.clone()));
            assert_eq!(frame[0], 0xc2);
            assert!(frame.len() < 100);

            let frame = encode(&codec, Message::Continuation(Item::FirstText("a".into())));
            assert_eq!(frame[0], 0x41);
            let frame = encode(&codec, Message::Continuation(Item::Continue("b".into())));
            assert_eq!(frame[0], 0x00);
            let frame = encode(&codec, Message::Continuation(Item::Last("c".into())));
            assert_eq!(frame[0], 0x80);

            // control frames are not compressed
            let frame = encode(&codec, Message::Ping("ping".into()));
            assert_eq!(&frame[..], b"\x89\x04ping");
            let frame = encode(&codec, Message::Close(None));
            assert_eq!(&frame[..], &[0x88, 0x00]);

            // binary pages are compressed
            let mut dst = BytePages::default();
            server()
                .encode_page(BytePage::from(data.clone()), &mut dst)
                .unwrap();
            let mut src = BytesMut::from(&Bytes::from(dst)[..]);
            assert_eq!(src[0], 0xc2);
            assert_eq!(
                client().decode(&mut src).unwrap(),
                Some(Frame::Binary(data))
            );
        }

        /// STATEMENT: Negotiating deflate still permits uncompressed whole and fragmented messages.
        ///
        /// HOW:
        /// - Decode a buffer mixing uncompressed text/fragments/binary and an empty compressed message.
        /// - Compare the complete expected frame sequence.
        #[test]
        fn uncompressed_messages() {
            // the peer may send uncompressed messages
            let codec = client();
            let frames = decode_all(
                &codec,
                BytesMut::from(&b"\x81\x05Hello\x01\x01a\x80\x01b\xc1\x01\x00\x82\x01c"[..]),
            );
            assert_eq!(
                frames,
                [
                    text("Hello"),
                    Frame::Continuation(Item::FirstText("a".into())),
                    Frame::Continuation(Item::Last("b".into())),
                    text(""),
                    Frame::Binary("c".into()),
                ]
            );
        }

        /// STATEMENT: Deflate does not permit RSV1 on controls/continuations or any use of RSV2/RSV3.
        ///
        /// HOW:
        /// - Decode illegal flag combinations and inspect reserved-bit errors.
        /// - Verify RSV1 is also rejected when compression was not negotiated.
        #[test]
        fn reserved_bits() {
            for frame in [
                // RSV1 on control frames and continuation frames
                &b"\xc9\x00"[..],
                b"\xca\x00",
                b"\xc8\x00",
                // RSV2 and RSV3 are not used by the extension
                b"\xa1\x00",
                b"\x91\x00",
                b"\xe1\x00",
            ] {
                assert!(
                    matches!(
                        decode(&client(), frame),
                        Err(ProtocolError::ReservedBits(_))
                    ),
                    "{frame:?}"
                );
            }

            let codec = client();
            assert!(matches!(
                decode(&codec, b"\x01\x01a"),
                Ok(Some(Frame::Continuation(Item::FirstText(_))))
            ));
            assert!(matches!(
                decode(&codec, b"\xc0\x01\x00"),
                Err(ProtocolError::ReservedBits(0b100))
            ));

            // RSV1 is reserved without the extension
            assert!(matches!(
                decode(
                    &Codec::new().set_client_mode(),
                    &[0xc1, 0x07, 0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00]
                ),
                Err(ProtocolError::ReservedBits(0b100))
            ));
        }

        /// STATEMENT: Invalid compressed data is rejected in both complete and initial-fragment frames.
        ///
        /// HOW:
        /// - Decode malformed deflate bytes with final and nonfinal text headers and compare protocol errors.
        #[test]
        fn invalid_data() {
            assert!(matches!(
                decode(&client(), b"\xc1\x02\xff\xff"),
                Err(ProtocolError::InvalidCompressedData)
            ));
            assert!(matches!(
                decode(&client(), b"\x41\x02\xff\xff"),
                Err(ProtocolError::InvalidCompressedData)
            ));
        }

        /// STATEMENT: Frame-size limits apply to decompressed bytes, independently for each fragment.
        ///
        /// HOW:
        /// - Decode a highly compressed payload at and below its expanded-size limit.
        /// - Accept a boundary-sized first fragment and reject an oversized last fragment.
        #[test]
        fn decompressed_size_limit() {
            let data = Bytes::from(vec![0; 1024 * 1024]);
            let frame = encode(&server(), Message::Binary(data.clone()));
            assert!(frame.len() < 2048);

            let codec = Codec::new()
                .set_client_mode()
                .max_size(data.len() - 1)
                .set_deflate(DeflateConfig::new());
            assert!(matches!(
                codec.decode(&mut frame.clone()),
                Err(ProtocolError::Overflow)
            ));

            let codec = Codec::new()
                .set_client_mode()
                .max_size(data.len())
                .set_deflate(DeflateConfig::new());
            assert_eq!(
                codec.decode(&mut frame.clone()).unwrap(),
                Some(Frame::Binary(data))
            );

            // the limit applies to each fragment
            let codec = Codec::new()
                .set_client_mode()
                .max_size(1000)
                .set_deflate(DeflateConfig::new());
            let sender = server();
            let mut src = encode(
                &sender,
                Message::Continuation(Item::FirstBinary(vec![0; 1000].into())),
            );
            src.extend_from_slice(&encode(
                &sender,
                Message::Continuation(Item::Last(vec![0; 1001].into())),
            ));
            assert!(matches!(
                codec.decode(&mut src),
                Ok(Some(Frame::Continuation(Item::FirstBinary(_))))
            ));
            assert!(matches!(
                codec.decode(&mut src),
                Err(ProtocolError::Overflow)
            ));
        }

        /// STATEMENT: Message size limits and UTF-8 validation apply to decompressed payloads.
        ///
        /// HOW:
        /// - Send two compressed 1000-byte fragments to a codec with `max_message_size(1500)` and expect `Overflow` on the second.
        /// - Send a compressed text fragment holding `0xff` and expect `InvalidUtf8`.
        #[test]
        fn message_limits_after_decompression() {
            let sender = server();
            let codec = client().max_message_size(1500);
            let mut src = encode(
                &sender,
                Message::Continuation(Item::FirstBinary(vec![0; 1000].into())),
            );
            src.extend_from_slice(&encode(
                &sender,
                Message::Continuation(Item::Last(vec![0; 1000].into())),
            ));
            assert!(src.len() < 1500);
            assert!(matches!(
                codec.decode(&mut src),
                Ok(Some(Frame::Continuation(Item::FirstBinary(_))))
            ));
            assert!(matches!(
                codec.decode(&mut src),
                Err(ProtocolError::Overflow)
            ));

            let src = encode(
                &server(),
                Message::Continuation(Item::FirstText(Bytes::from_static(&[0xff]))),
            );
            assert!(matches!(
                client().decode(&mut src.clone()),
                Err(ProtocolError::InvalidUtf8)
            ));
        }

        /// STATEMENT: Negotiated server context resets reject cross-message references without resetting the client's compressor.
        ///
        /// HOW:
        /// - Compare repeated server frames and decode them with fresh contexts, then reject a back-reference-only message.
        /// - Verify repeated client messages shrink through retained compression history.
        #[test]
        fn context_takeover() {
            // the server resets its context, the client reads each message
            // with a new context
            let cfg = DeflateConfig::new().set_server_no_context_takeover(true);
            let server = Codec::new().set_deflate(cfg.clone());
            let client = Codec::new().set_client_mode().set_deflate(cfg);
            let first = encode(&server, Message::Text("Hello".into()));
            assert_eq!(encode(&server, Message::Text("Hello".into())), first);
            let mut src = first.clone();
            src.extend_from_slice(&first);
            assert_eq!(decode_all(&client, src), [text("Hello"), text("Hello")]);

            // a message that refers to the previous one
            assert!(matches!(
                decode(&client, &[0xc1, 0x05, 0xf2, 0x00, 0x11, 0x00, 0x00]),
                Err(ProtocolError::InvalidCompressedData)
            ));

            // the client keeps its context
            let first = encode(&client, Message::Text("Hello".into()));
            assert!(encode(&client, Message::Text("Hello".into())).len() < first.len());
        }

        /// STATEMENT: Cloned WebSocket codecs share negotiated compression configuration and history.
        ///
        /// HOW:
        /// - Inspect clone/default configuration, encode consecutive messages through separate clones, and decode both together.
        #[test]
        fn clones_share_state() {
            let (client, server) = (client(), server());
            let server2 = server.clone();
            assert_eq!(server2.deflate(), Some(&DeflateConfig::new()));
            assert_eq!(Codec::new().deflate(), None);

            let mut src = encode(&server, Message::Text("Hello".into()));
            src.extend_from_slice(&encode(&server2, Message::Text("Hello".into())));
            assert_eq!(decode_all(&client, src), [text("Hello"), text("Hello")]);
        }
    }
}
