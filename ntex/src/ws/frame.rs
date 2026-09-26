use nanorand::Rng;

use super::proto::{CloseCode, CloseReason, OpCode};
use super::{error::ProtocolError, mask::apply_mask};
use crate::util::{BufMut, BytePage, BytePages, Bytes, BytesMut};

/// WebSocket frame parser.
#[derive(Debug)]
pub struct Parser;

impl Parser {
    fn parse_metadata(
        src: &[u8],
        server: bool,
        max_size: usize,
    ) -> Result<Option<(usize, bool, OpCode, usize, Option<u32>)>, ProtocolError> {
        let chunk_len = src.len();

        let mut idx = 2;
        if chunk_len < 2 {
            return Ok(None);
        }

        let first = src[0];
        let second = src[1];
        let finished = first & 0x80 != 0;
        let reserved = first & 0x70;
        if reserved != 0 {
            return Err(ProtocolError::ReservedBits(reserved >> 4));
        }

        // check masking
        let masked = second & 0x80 != 0;
        if !masked && server {
            return Err(ProtocolError::UnmaskedFrame);
        } else if masked && !server {
            return Err(ProtocolError::MaskedFrame);
        }

        // Op code
        let raw_opcode = first & 0x0F;
        let opcode =
            OpCode::try_from(raw_opcode).map_err(|()| ProtocolError::InvalidOpcode(raw_opcode))?;
        if opcode.is_control() && !finished {
            return Err(ProtocolError::FragmentedControlFrame(opcode));
        }

        let len = second & 0x7F;
        let length = if len == 126 {
            if chunk_len < 4 {
                return Ok(None);
            }
            let len = usize::from(u16::from_be_bytes(
                TryFrom::try_from(&src[idx..idx + 2]).unwrap(),
            ));
            if len < 126 {
                return Err(ProtocolError::InvalidLengthEncoding);
            }
            idx += 2;
            len
        } else if len == 127 {
            if chunk_len < 10 {
                return Ok(None);
            }
            if src[idx] & 0x80 != 0 {
                return Err(ProtocolError::InvalidLengthEncoding);
            }
            let len = u64::from_be_bytes(TryFrom::try_from(&src[idx..idx + 8]).unwrap());
            if len < 65_536 {
                return Err(ProtocolError::InvalidLengthEncoding);
            }
            if len > max_size as u64 {
                return Err(ProtocolError::Overflow);
            }
            idx += 8;
            len as usize
        } else {
            len as usize
        };

        if opcode.is_control() && length > 125 {
            return Err(ProtocolError::InvalidLength(length));
        }

        // check for max allowed size
        if length > max_size {
            return Err(ProtocolError::Overflow);
        }

        let mask = if server {
            if chunk_len < idx + 4 {
                return Ok(None);
            }

            let mask = u32::from_ne_bytes(TryFrom::try_from(&src[idx..idx + 4]).unwrap());
            idx += 4;
            Some(mask)
        } else {
            None
        };

        Ok(Some((idx, finished, opcode, length, mask)))
    }

    /// Parses one WebSocket frame from `src`.
    ///
    /// `server` selects the expected masking direction: server-side parsing
    /// requires masked frames, while client-side parsing rejects them.
    /// `max_size` limits the frame payload size.
    ///
    /// Returns the final-fragment flag, opcode, and optional payload when a
    /// complete frame is available. Returns [`None`] without consuming a
    /// partial frame.
    pub fn parse(
        src: &mut BytesMut,
        server: bool,
        max_size: usize,
    ) -> Result<Option<(bool, OpCode, Option<Bytes>)>, ProtocolError> {
        // try to parse ws frame metadata
        let Some((idx, finished, opcode, length, mask)) =
            Parser::parse_metadata(src, server, max_size)?
        else {
            return Ok(None);
        };

        // not enough data
        if src.len() < idx + length {
            return Ok(None);
        }

        // remove prefix
        src.advance_to(idx);

        // no need for body
        if length == 0 {
            return Ok(Some((finished, opcode, None)));
        }

        // unmask
        if let Some(mask) = mask {
            apply_mask(&mut src[..length], mask);
        }

        Ok(Some((finished, opcode, Some(src.split_to(length)))))
    }

    /// Parses a close-frame payload.
    ///
    /// Returns [`None`] for an empty payload.
    ///
    /// # Errors
    ///
    /// Returns an error for a one-byte payload, an invalid close status code,
    /// or a description that is not valid UTF-8.
    pub fn parse_close_payload(payload: &[u8]) -> Result<Option<CloseReason>, ProtocolError> {
        if payload.is_empty() {
            return Ok(None);
        }

        if payload.len() == 1 {
            return Err(ProtocolError::InvalidClosePayload);
        }

        let raw_code = u16::from_be_bytes(TryFrom::try_from(&payload[..2]).unwrap());
        let code = CloseCode::from(raw_code);
        if !code.is_valid() {
            return Err(ProtocolError::InvalidCloseCode(raw_code));
        }
        let description = if payload.len() > 2 {
            Some(
                std::str::from_utf8(&payload[2..])
                    .map_err(|_| ProtocolError::InvalidUtf8)?
                    .to_owned(),
            )
        } else {
            None
        };
        Ok(Some(CloseReason { code, description }))
    }

    /// Encodes a WebSocket frame into `dst`.
    ///
    /// `fin` controls the final-fragment bit and `mask` controls whether a new
    /// random masking key is applied.
    ///
    /// # Errors
    ///
    /// Returns an error if a control frame is fragmented or has a payload
    /// larger than 125 bytes.
    pub fn write_message<B>(
        dst: &mut BytePages,
        pl: B,
        op: OpCode,
        fin: bool,
        mask: bool,
    ) -> Result<(), ProtocolError>
    where
        BytePage: From<B>,
    {
        let payload = BytePage::from(pl);
        if op.is_control() {
            if !fin {
                return Err(ProtocolError::FragmentedControlFrame(op));
            }
            if payload.len() > 125 {
                return Err(ProtocolError::InvalidLength(payload.len()));
            }
        }

        let one: u8 = if fin {
            0x80 | Into::<u8>::into(op)
        } else {
            op.into()
        };
        let payload_len = payload.len();
        let two = if mask { 0x80 } else { 0 };

        if payload_len < 126 {
            dst.extend_from_slice(&[one, two | payload_len as u8]);
        } else if payload_len <= 65_535 {
            dst.extend_from_slice(&[one, two | 0x007e]);
            dst.put_u16(payload_len as u16);
        } else {
            dst.extend_from_slice(&[one, two | 127]);
            dst.put_u64(payload_len as u64);
        }

        if mask {
            let mask: u32 = nanorand::tls_rng().generate();
            let mut buf = BytesMut::from(payload);
            apply_mask(&mut buf, mask);
            dst.extend_from_slice(&mask.to_ne_bytes());
            dst.append::<BytesMut>(buf);
        } else {
            dst.append::<BytePage>(payload);
        }
        Ok(())
    }

    /// Encodes a final close control frame into `dst`.
    ///
    /// # Errors
    ///
    /// Returns an error if the close code cannot be sent or the encoded close
    /// payload would exceed 125 bytes.
    #[inline]
    pub fn write_close(
        dst: &mut BytePages,
        reason: Option<CloseReason>,
        mask: bool,
    ) -> Result<(), ProtocolError> {
        let payload = match reason {
            None => Bytes::new(),
            Some(reason) => {
                if !reason.code.is_valid() || (!mask && matches!(reason.code, CloseCode::Extension))
                {
                    return Err(ProtocolError::InvalidCloseCode(reason.code.into()));
                }
                let mut payload =
                    BytesMut::with_capacity(reason.description.as_ref().map_or(0, String::len) + 2);
                payload.put_u16(u16::from(reason.code));
                if let Some(description) = reason.description {
                    payload.extend_from_slice(description.as_bytes());
                }
                payload.freeze()
            }
        };

        Parser::write_message(dst, payload, OpCode::Close, true, mask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct F {
        finished: bool,
        opcode: OpCode,
        payload: Bytes,
    }

    fn is_none(frm: &Result<Option<(bool, OpCode, Option<Bytes>)>, ProtocolError>) -> bool {
        matches!(*frm, Ok(None))
    }

    fn extract(frm: Result<Option<(bool, OpCode, Option<Bytes>)>, ProtocolError>) -> F {
        match frm {
            Ok(Some((finished, opcode, payload))) => F {
                finished,
                opcode,
                payload: payload.unwrap_or_else(Bytes::new),
            },
            _ => unreachable!("error"),
        }
    }

    #[test]
    fn test_parse() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0001u8][..]);
        assert!(is_none(&Parser::parse(&mut buf, false, 1024)));

        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0001u8][..]);
        buf.extend(b"1");

        let frame = extract(Parser::parse(&mut buf, false, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload.as_ref(), &b"1"[..]);
    }

    #[test]
    fn test_parse_length0() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0000u8][..]);
        let frame = extract(Parser::parse(&mut buf, false, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn test_reserved_bits() {
        for reserved in [0x10, 0x20, 0x40, 0x70] {
            let mut buf = BytesMut::from(&[0x81 | reserved, 0][..]);
            assert!(matches!(
                Parser::parse(&mut buf, false, 1024),
                Err(ProtocolError::ReservedBits(_))
            ));
        }
    }

    #[test]
    fn test_invalid_control_frames() {
        let mut fragmented = BytesMut::from(&[0x09, 0][..]);
        assert!(matches!(
            Parser::parse(&mut fragmented, false, 1024),
            Err(ProtocolError::FragmentedControlFrame(OpCode::Ping))
        ));

        let mut oversized = BytesMut::from(&[0x88, 126, 0, 126][..]);
        assert!(matches!(
            Parser::parse(&mut oversized, false, 1024),
            Err(ProtocolError::InvalidLength(126))
        ));
    }

    #[test]
    fn test_parse_length2() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 126u8][..]);
        assert!(is_none(&Parser::parse(&mut buf, false, 1024)));

        let mut buf = BytesMut::from(&[0b0000_0001u8, 126u8][..]);
        buf.extend(&[0u8, 126u8][..]);
        buf.extend(vec![1; 126]);

        let frame = extract(Parser::parse(&mut buf, false, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload.len(), 126);
    }

    #[test]
    fn test_parse_length4() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 127u8][..]);
        assert!(is_none(&Parser::parse(&mut buf, false, 1024)));

        let mut buf = BytesMut::from(&[0b0000_0001u8, 127u8][..]);
        buf.extend(&[0u8, 0u8, 0u8, 0u8, 0u8, 1u8, 0u8, 0u8][..]);
        buf.extend(vec![1; 65_536]);

        let frame = extract(Parser::parse(&mut buf, false, 65_536));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload.len(), 65_536);
    }

    #[test]
    fn test_noncanonical_lengths() {
        let mut short = BytesMut::from(&[0x82, 126, 0, 125][..]);
        assert!(matches!(
            Parser::parse(&mut short, false, usize::MAX),
            Err(ProtocolError::InvalidLengthEncoding)
        ));

        let mut medium = BytesMut::from(&[0x82, 127, 0, 0, 0, 0, 0, 0, 0xff, 0xff][..]);
        assert!(matches!(
            Parser::parse(&mut medium, false, usize::MAX),
            Err(ProtocolError::InvalidLengthEncoding)
        ));

        let mut high_bit = BytesMut::from(&[0x82, 127, 0x80, 0, 0, 0, 0, 1, 0, 0][..]);
        assert!(matches!(
            Parser::parse(&mut high_bit, false, usize::MAX),
            Err(ProtocolError::InvalidLengthEncoding)
        ));
    }

    #[test]
    fn test_parse_frame_mask() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b1000_0001u8][..]);
        buf.extend(b"0001");
        buf.extend(b"1");

        assert!(Parser::parse(&mut buf, false, 1024).is_err());

        let frame = extract(Parser::parse(&mut buf, true, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload, Bytes::from(vec![1u8]));
    }

    #[test]
    fn test_parse_frame_no_mask() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0001u8][..]);
        buf.extend([1u8]);

        assert!(Parser::parse(&mut buf, true, 1024).is_err());

        let frame = extract(Parser::parse(&mut buf, false, 1024));
        assert!(!frame.finished);
        assert_eq!(frame.opcode, OpCode::Text);
        assert_eq!(frame.payload, Bytes::from(vec![1u8]));
    }

    #[test]
    fn test_parse_frame_max_size() {
        let mut buf = BytesMut::from(&[0b0000_0001u8, 0b0000_0010u8][..]);
        buf.extend([1u8, 1u8]);

        assert!(Parser::parse(&mut buf, true, 1).is_err());

        if let Err(ProtocolError::Overflow) = Parser::parse(&mut buf, false, 0) {
        } else {
            unreachable!("error");
        }
    }

    #[test]
    fn test_masked_frames_roundtrip_with_distinct_masks() {
        let mut masks = Vec::new();
        for _ in 0..4 {
            let mut pages = BytePages::default();
            Parser::write_message(&mut pages, Bytes::from("data"), OpCode::Binary, true, true)
                .unwrap();
            let mut buf = BytesMut::from(&Bytes::from(pages)[..]);
            masks.push(buf[2..6].to_vec());

            let frame = extract(Parser::parse(&mut buf, true, 1024));
            assert!(frame.finished);
            assert_eq!(frame.opcode, OpCode::Binary);
            assert_eq!(frame.payload, Bytes::from("data"));
        }
        masks.dedup();
        assert!(masks.len() > 1);
    }

    #[test]
    fn test_ping_frame() {
        let mut buf = BytePages::default();
        Parser::write_message(&mut buf, Bytes::from("data"), OpCode::Ping, true, false).unwrap();

        let mut v = vec![137u8, 4u8];
        v.extend(b"data");
        assert_eq!(&Bytes::from(buf)[..], &v[..]);
    }

    #[test]
    fn test_pong_frame() {
        let mut buf = BytePages::default();
        Parser::write_message(&mut buf, Bytes::from("data"), OpCode::Pong, true, false).unwrap();

        let mut v = vec![138u8, 4u8];
        v.extend(b"data");
        assert_eq!(&Bytes::from(buf)[..], &v[..]);
    }

    #[test]
    fn test_close_frame() {
        let mut buf = BytePages::default();
        let reason = (CloseCode::Normal, "data");
        Parser::write_close(&mut buf, Some(reason.into()), false).unwrap();

        let mut v = vec![136u8, 6u8, 3u8, 232u8];
        v.extend(b"data");
        assert_eq!(&Bytes::from(buf)[..], &v[..]);
    }

    #[test]
    fn test_empty_close_frame() {
        let mut buf = BytePages::default();
        Parser::write_close(&mut buf, None, false).unwrap();
        assert_eq!(&Bytes::from(buf)[..], &[0x88, 0x00]);
    }

    #[test]
    fn test_close_validation() {
        assert!(matches!(
            Parser::parse_close_payload(&[1]),
            Err(ProtocolError::InvalidClosePayload)
        ));
        assert!(matches!(
            Parser::parse_close_payload(&1006u16.to_be_bytes()),
            Err(ProtocolError::InvalidCloseCode(1006))
        ));
        assert!(matches!(
            Parser::parse_close_payload(&[0x03, 0xe8, 0xff]),
            Err(ProtocolError::InvalidUtf8)
        ));

        let mut buf = BytePages::default();
        assert!(matches!(
            Parser::write_message(
                &mut buf,
                Bytes::from(vec![0; 126]),
                OpCode::Ping,
                true,
                false
            ),
            Err(ProtocolError::InvalidLength(126))
        ));
        assert!(matches!(
            Parser::write_message(&mut buf, Bytes::new(), OpCode::Pong, false, false),
            Err(ProtocolError::FragmentedControlFrame(OpCode::Pong))
        ));
        assert!(matches!(
            Parser::write_close(
                &mut buf,
                Some(CloseReason {
                    code: CloseCode::Other(2000),
                    description: None,
                }),
                false
            ),
            Err(ProtocolError::InvalidCloseCode(2000))
        ));
        assert!(matches!(
            Parser::write_close(
                &mut buf,
                Some(CloseReason {
                    code: CloseCode::Extension,
                    description: None,
                }),
                false
            ),
            Err(ProtocolError::InvalidCloseCode(1010))
        ));
        assert!(matches!(
            Parser::write_close(
                &mut buf,
                Some(CloseReason {
                    code: CloseCode::Normal,
                    description: Some("x".repeat(124)),
                }),
                false
            ),
            Err(ProtocolError::InvalidLength(126))
        ));
    }
}
