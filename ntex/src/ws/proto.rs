use std::fmt;

use base64::{Engine, engine::general_purpose::STANDARD as base64};

use super::error::HandshakeError;

/// WebSocket frame operation codes defined by RFC 6455.
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum OpCode {
    /// Indicates a continuation frame of a fragmented message.
    Continue,
    /// Indicates a text data frame.
    Text,
    /// Indicates a binary data frame.
    Binary,
    /// Indicates a close control frame.
    Close,
    /// Indicates a ping control frame.
    Ping,
    /// Indicates a pong control frame.
    Pong,
}

impl fmt::Display for OpCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            OpCode::Continue => write!(f, "CONTINUE"),
            OpCode::Text => write!(f, "TEXT"),
            OpCode::Binary => write!(f, "BINARY"),
            OpCode::Close => write!(f, "CLOSE"),
            OpCode::Ping => write!(f, "PING"),
            OpCode::Pong => write!(f, "PONG"),
        }
    }
}

impl From<OpCode> for u8 {
    fn from(code: OpCode) -> u8 {
        match code {
            OpCode::Continue => 0,
            OpCode::Text => 1,
            OpCode::Binary => 2,
            OpCode::Close => 8,
            OpCode::Ping => 9,
            OpCode::Pong => 10,
        }
    }
}

impl TryFrom<u8> for OpCode {
    type Error = ();

    fn try_from(byte: u8) -> Result<OpCode, Self::Error> {
        match byte {
            0 => Ok(OpCode::Continue),
            1 => Ok(OpCode::Text),
            2 => Ok(OpCode::Binary),
            8 => Ok(OpCode::Close),
            9 => Ok(OpCode::Ping),
            10 => Ok(OpCode::Pong),
            _ => Err(()),
        }
    }
}

impl OpCode {
    pub(crate) fn is_control(self) -> bool {
        matches!(self, OpCode::Close | OpCode::Ping | OpCode::Pong)
    }
}

/// Status code used to indicate why an endpoint is closing the `WebSocket`
/// connection.
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum CloseCode {
    /// Indicates a normal closure, meaning that the purpose for
    /// which the connection was established has been fulfilled.
    Normal,
    /// Indicates that an endpoint is "going away", such as a server
    /// going down or a browser having navigated away from a page.
    Away,
    /// Indicates that an endpoint is terminating the connection due
    /// to a protocol error.
    Protocol,
    /// Indicates that an endpoint is terminating the connection
    /// because it has received a type of data it cannot accept (e.g., an
    /// endpoint that understands only text data MAY send this if it
    /// receives a binary message).
    Unsupported,
    /// Indicates that the connection closed without a close control frame.
    ///
    /// This code is reserved for local reporting and cannot be sent in a
    /// close frame.
    Abnormal,
    /// Indicates that an endpoint is terminating the connection
    /// because it has received data within a message that was not
    /// consistent with the type of the message (e.g., non-UTF-8 \[RFC3629\]
    /// data within a text message).
    Invalid,
    /// Indicates that an endpoint is terminating the connection
    /// because it has received a message that violates its policy.  This
    /// is a generic status code that can be returned when there is no
    /// other more suitable status code (e.g., Unsupported or Size) or if there
    /// is a need to hide specific details about the policy.
    Policy,
    /// Indicates that an endpoint is terminating the connection
    /// because it has received a message that is too big for it to
    /// process.
    Size,
    /// Indicates that an endpoint (client) is terminating the
    /// connection because it has expected the server to negotiate one or
    /// more extension, but the server didn't return them in the response
    /// message of the WebSocket handshake.  The list of extensions that
    /// are needed should be given as the reason for closing.
    /// Note that this status code is not used by the server, because it
    /// can fail the WebSocket handshake instead.
    Extension,
    /// Indicates that a server is terminating the connection because
    /// it encountered an unexpected condition that prevented it from
    /// fulfilling the request.
    Error,
    /// Indicates that the server is restarting. A client may choose to
    /// reconnect, and if it does, it should use a randomized delay of 5-30
    /// seconds between attempts.
    Restart,
    /// Indicates that the server is overloaded and the client should either
    /// connect to a different IP (when multiple targets exist), or
    /// reconnect to the same IP when a user has performed an action.
    Again,
    /// Indicates that an upstream server returned an invalid response.
    BadGateway,
    /// Indicates that the TLS handshake failed.
    ///
    /// This code is reserved for local reporting and cannot be sent in a
    /// close frame.
    Tls,
    /// An unrecognized or application-defined close code.
    ///
    /// Only values in the range 3000 through 4999 can be sent.
    Other(u16),
}

impl From<CloseCode> for u16 {
    fn from(code: CloseCode) -> u16 {
        match code {
            CloseCode::Normal => 1000,
            CloseCode::Away => 1001,
            CloseCode::Protocol => 1002,
            CloseCode::Unsupported => 1003,
            CloseCode::Abnormal => 1006,
            CloseCode::Invalid => 1007,
            CloseCode::Policy => 1008,
            CloseCode::Size => 1009,
            CloseCode::Extension => 1010,
            CloseCode::Error => 1011,
            CloseCode::Restart => 1012,
            CloseCode::Again => 1013,
            CloseCode::BadGateway => 1014,
            CloseCode::Tls => 1015,
            CloseCode::Other(code) => code,
        }
    }
}

impl From<u16> for CloseCode {
    fn from(code: u16) -> CloseCode {
        match code {
            1000 => CloseCode::Normal,
            1001 => CloseCode::Away,
            1002 => CloseCode::Protocol,
            1003 => CloseCode::Unsupported,
            1006 => CloseCode::Abnormal,
            1007 => CloseCode::Invalid,
            1008 => CloseCode::Policy,
            1009 => CloseCode::Size,
            1010 => CloseCode::Extension,
            1011 => CloseCode::Error,
            1012 => CloseCode::Restart,
            1013 => CloseCode::Again,
            1014 => CloseCode::BadGateway,
            1015 => CloseCode::Tls,
            _ => CloseCode::Other(code),
        }
    }
}

impl CloseCode {
    pub(crate) fn is_valid(self) -> bool {
        !matches!(self, CloseCode::Abnormal | CloseCode::Tls)
            && !matches!(self, CloseCode::Other(code) if !(3000..=4999).contains(&code))
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
/// Reason supplied in a WebSocket close control frame.
pub struct CloseReason {
    /// Close status code.
    pub code: CloseCode,
    /// Optional human-readable description.
    pub description: Option<String>,
}

impl From<CloseCode> for CloseReason {
    fn from(code: CloseCode) -> Self {
        CloseReason {
            code,
            description: None,
        }
    }
}

impl<T: Into<String>> From<(CloseCode, T)> for CloseReason {
    fn from(info: (CloseCode, T)) -> Self {
        CloseReason {
            code: info.0,
            description: Some(info.1.into()),
        }
    }
}

// SHA-1 hashing algorithm initial hash values.
const H0: u32 = 0x6745_2301;
const H1: u32 = 0xEFCD_AB89;
const H2: u32 = 0x98BA_DCFE;
const H3: u32 = 0x1032_5476;
const H4: u32 = 0xC3D2_E1F0;
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[allow(clippy::many_single_char_names)]
/// Computes the `Sec-WebSocket-Accept` value for a client handshake key.
///
/// # Errors
///
/// Returns [`HandshakeError::BadWebsocketKey`] when `key` is longer than
/// 32 bytes.
pub fn hash_key(key: &[u8]) -> Result<String, HandshakeError> {
    if key.len() > 32 {
        return Err(HandshakeError::BadWebsocketKey);
    }
    let mut input = [0; 192];
    let klen = key.len();
    let len = klen + 36;
    input[..klen].copy_from_slice(key);
    input[klen..len].copy_from_slice(WS_GUID.as_bytes());

    // Initialize variables to the SHA-1's initial hash values.
    let (mut h0, mut h1, mut h2, mut h3, mut h4) = (H0, H1, H2, H3, H4);
    let (mut a, mut b, mut c, mut d, mut e);

    // Pad the key
    let msg = pad_message(len, &mut input);

    // Process each 512-bit chunk of the padded message.
    for chunk in msg.chunks(64) {
        // Get the message schedule
        let mut schedule = [0u32; 80];
        for (i, block) in chunk.chunks(4).enumerate() {
            schedule[i] = u32::from_be_bytes(block.try_into().unwrap());
        }
        for i in 16..80 {
            schedule[i] = schedule[i - 3] ^ schedule[i - 8] ^ schedule[i - 14] ^ schedule[i - 16];
            schedule[i] = schedule[i].rotate_left(1);
        }

        a = h0;
        b = h1;
        c = h2;
        d = h3;
        e = h4;

        // Main loop of the SHA-1 algorithm
        for (i, sch) in schedule.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };

            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*sch);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        // Add the compressed chunk to the current hash value.
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut hash = [0u8; 20];
    hash[0..4].copy_from_slice(&h0.to_be_bytes());
    hash[4..8].copy_from_slice(&h1.to_be_bytes());
    hash[8..12].copy_from_slice(&h2.to_be_bytes());
    hash[12..16].copy_from_slice(&h3.to_be_bytes());
    hash[16..20].copy_from_slice(&h4.to_be_bytes());

    Ok(base64.encode(hash))
}

fn pad_message(len: usize, input: &mut [u8]) -> &[u8] {
    let mut cur = len + 1;
    let bit_length = len as u64 * 8;

    input[len] = 0x80;
    while (cur * 8) % 512 != 448 {
        input[cur] = 0;
        cur += 1;
    }
    let orig_len = &bit_length.to_be_bytes();
    let total = cur + orig_len.len();
    input[cur..total].copy_from_slice(orig_len);
    &input[..total]
}

#[cfg(test)]
#[allow(unused_imports, unused_variables, dead_code)]
mod tests {
    use super::*;

    macro_rules! opcode_into {
        ($from:expr => $opcode:pat) => {
            match OpCode::try_from($from).unwrap() {
                e @ $opcode => (),
                e => unreachable!("{:?}", e),
            }
        };
    }

    macro_rules! opcode_from {
        ($from:expr => $opcode:pat) => {
            let res: u8 = $from.into();
            match res {
                e @ $opcode => (),
                e => unreachable!("{:?}", e),
            }
        };
    }

    #[test]
    fn test_to_opcode() {
        opcode_into!(0 => OpCode::Continue);
        opcode_into!(1 => OpCode::Text);
        opcode_into!(2 => OpCode::Binary);
        opcode_into!(8 => OpCode::Close);
        opcode_into!(9 => OpCode::Ping);
        opcode_into!(10 => OpCode::Pong);
        assert!(OpCode::try_from(99).is_err());
    }

    #[test]
    fn test_from_opcode() {
        opcode_from!(OpCode::Continue => 0);
        opcode_from!(OpCode::Text => 1);
        opcode_from!(OpCode::Binary => 2);
        opcode_from!(OpCode::Close => 8);
        opcode_from!(OpCode::Ping => 9);
        opcode_from!(OpCode::Pong => 10);
    }

    #[test]
    fn test_from_opcode_display() {
        assert_eq!(format!("{}", OpCode::Continue), "CONTINUE");
        assert_eq!(format!("{}", OpCode::Text), "TEXT");
        assert_eq!(format!("{}", OpCode::Binary), "BINARY");
        assert_eq!(format!("{}", OpCode::Close), "CLOSE");
        assert_eq!(format!("{}", OpCode::Ping), "PING");
        assert_eq!(format!("{}", OpCode::Pong), "PONG");
    }

    #[test]
    fn test_hash_key() {
        let hash = hash_key(b"hello actix-web").unwrap();
        assert_eq!(&hash, "cR1dlyUUJKp0s/Bel25u5TgvC3E=");
    }

    #[test]
    fn closecode_from_u16() {
        assert_eq!(CloseCode::from(1000u16), CloseCode::Normal);
        assert_eq!(CloseCode::from(1001u16), CloseCode::Away);
        assert_eq!(CloseCode::from(1002u16), CloseCode::Protocol);
        assert_eq!(CloseCode::from(1003u16), CloseCode::Unsupported);
        assert_eq!(CloseCode::from(1006u16), CloseCode::Abnormal);
        assert_eq!(CloseCode::from(1007u16), CloseCode::Invalid);
        assert_eq!(CloseCode::from(1008u16), CloseCode::Policy);
        assert_eq!(CloseCode::from(1009u16), CloseCode::Size);
        assert_eq!(CloseCode::from(1010u16), CloseCode::Extension);
        assert_eq!(CloseCode::from(1011u16), CloseCode::Error);
        assert_eq!(CloseCode::from(1012u16), CloseCode::Restart);
        assert_eq!(CloseCode::from(1013u16), CloseCode::Again);
        assert_eq!(CloseCode::from(1014u16), CloseCode::BadGateway);
        assert_eq!(CloseCode::from(1015u16), CloseCode::Tls);
        assert_eq!(CloseCode::from(3000u16), CloseCode::Other(3000));
        assert!(!CloseCode::from(1005u16).is_valid());
        assert!(!CloseCode::from(1006u16).is_valid());
        assert!(!CloseCode::from(1015u16).is_valid());
        assert!(!CloseCode::from(2000u16).is_valid());
    }

    #[test]
    fn closecode_into_u16() {
        assert_eq!(1000u16, Into::<u16>::into(CloseCode::Normal));
        assert_eq!(1001u16, Into::<u16>::into(CloseCode::Away));
        assert_eq!(1002u16, Into::<u16>::into(CloseCode::Protocol));
        assert_eq!(1003u16, Into::<u16>::into(CloseCode::Unsupported));
        assert_eq!(1006u16, Into::<u16>::into(CloseCode::Abnormal));
        assert_eq!(1007u16, Into::<u16>::into(CloseCode::Invalid));
        assert_eq!(1008u16, Into::<u16>::into(CloseCode::Policy));
        assert_eq!(1009u16, Into::<u16>::into(CloseCode::Size));
        assert_eq!(1010u16, Into::<u16>::into(CloseCode::Extension));
        assert_eq!(1011u16, Into::<u16>::into(CloseCode::Error));
        assert_eq!(1012u16, Into::<u16>::into(CloseCode::Restart));
        assert_eq!(1013u16, Into::<u16>::into(CloseCode::Again));
        assert_eq!(1014u16, Into::<u16>::into(CloseCode::BadGateway));
        assert_eq!(1015u16, Into::<u16>::into(CloseCode::Tls));
        assert_eq!(3000u16, Into::<u16>::into(CloseCode::Other(3000)));
        assert!(!CloseCode::Other(2000).is_valid());
    }
}
