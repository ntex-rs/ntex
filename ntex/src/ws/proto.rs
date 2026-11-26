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

// SHA-1 initial hash values and round constants.
const H0: u32 = 0x6745_2301;
const H1: u32 = 0xEFCD_AB89;
const H2: u32 = 0x98BA_DCFE;
const H3: u32 = 0x1032_5476;
const H4: u32 = 0xC3D2_E1F0;
const K0: u32 = 0x5A82_7999;
const K1: u32 = 0x6ED9_EBA1;
const K2: u32 = 0x8F1B_BCDC;
const K3: u32 = 0xCA62_C1D6;
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

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
    // The key and GUID take at most 68 bytes, so the padded message
    // is one or two 64-byte blocks.
    let mut input = [0u8; 128];
    let len = key.len() + WS_GUID.len();
    input[..key.len()].copy_from_slice(key);
    input[key.len()..len].copy_from_slice(WS_GUID.as_bytes());
    input[len] = 0x80;
    let total = if len + 9 <= 64 { 64 } else { 128 };
    input[total - 8..total].copy_from_slice(&(len as u64 * 8).to_be_bytes());

    let mut h = [H0, H1, H2, H3, H4];
    for block in input[..total].as_chunks::<64>().0 {
        sha1_compress(&mut h, block);
    }

    let mut hash = [0u8; 20];
    for (dst, v) in hash.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *dst = v.to_be_bytes();
    }
    Ok(base64.encode(hash))
}

fn sha1_ch(b: u32, c: u32, d: u32) -> u32 {
    (b & c) | (!b & d)
}

fn sha1_parity(b: u32, c: u32, d: u32) -> u32 {
    b ^ c ^ d
}

fn sha1_maj(b: u32, c: u32, d: u32) -> u32 {
    (b & c) | (b & d) | (c & d)
}

/// Processes one 64-byte SHA-1 block.
///
/// The 80 rounds are unrolled and rotate the roles of `a`..`e` instead of
/// moving values between them; the message schedule is a 16-word ring.
#[allow(clippy::many_single_char_names)]
fn sha1_compress(h: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 16];
    for (w, b) in w.iter_mut().zip(block.as_chunks::<4>().0) {
        *w = u32::from_be_bytes(*b);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;

    // Message word for round `i`; from round 16 on it is computed in place.
    macro_rules! word {
        ($i:expr) => {{
            if $i >= 16 {
                w[$i & 15] = (w[($i + 13) & 15] ^ w[($i + 8) & 15] ^ w[($i + 2) & 15] ^ w[$i & 15])
                    .rotate_left(1);
            }
            w[$i & 15]
        }};
    }
    macro_rules! round {
        ($f:ident, $k:ident, $i:expr, $a:ident, $b:ident, $c:ident, $d:ident, $e:ident) => {
            $e = $e
                .wrapping_add($a.rotate_left(5))
                .wrapping_add($f($b, $c, $d))
                .wrapping_add($k)
                .wrapping_add(word!($i));
            $b = $b.rotate_left(30);
        };
    }
    // Five rounds bring the roles back to `a`..`e`.
    macro_rules! rounds5 {
        ($f:ident, $k:ident, $i:expr) => {
            round!($f, $k, $i, a, b, c, d, e);
            round!($f, $k, $i + 1, e, a, b, c, d);
            round!($f, $k, $i + 2, d, e, a, b, c);
            round!($f, $k, $i + 3, c, d, e, a, b);
            round!($f, $k, $i + 4, b, c, d, e, a);
        };
    }
    macro_rules! rounds20 {
        ($f:ident, $k:ident, $i:expr) => {
            rounds5!($f, $k, $i);
            rounds5!($f, $k, $i + 5);
            rounds5!($f, $k, $i + 10);
            rounds5!($f, $k, $i + 15);
        };
    }

    rounds20!(sha1_ch, K0, 0);
    rounds20!(sha1_parity, K1, 20);
    rounds20!(sha1_maj, K2, 40);
    rounds20!(sha1_parity, K3, 60);

    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
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

    /// STATEMENT: Wire opcode parsing recognizes supported frame types and rejects an unknown opcode.
    ///
    /// HOW:
    /// - Convert data/control opcode numbers and compare their variants, then check an invalid number fails.
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

    /// STATEMENT: Supported WebSocket opcode variants encode to their assigned wire values.
    ///
    /// HOW:
    /// - Convert continuation, text, binary, close, ping, and pong variants and compare numeric values.
    #[test]
    fn test_from_opcode() {
        opcode_from!(OpCode::Continue => 0);
        opcode_from!(OpCode::Text => 1);
        opcode_from!(OpCode::Binary => 2);
        opcode_from!(OpCode::Close => 8);
        opcode_from!(OpCode::Ping => 9);
        opcode_from!(OpCode::Pong => 10);
    }

    /// STATEMENT: WebSocket challenge hashing produces the expected accept value for one- and
    /// two-block SHA-1 inputs.
    ///
    /// HOW:
    /// - Hash the RFC 6455 sample key and keys of 0, 15, 19, 20 and 32 bytes; key and GUID
    ///   fit one padded block up to 19 bytes and need two from 20 bytes.
    /// - Compare each complete base64-encoded result with a reference SHA-1 value.
    #[test]
    fn test_hash_key() {
        let cases: [(&[u8], &str); 6] = [
            (b"", "Kfh9QIsMVZcl6xEPYxPHzW8SZ8w="),
            (b"hello actix-web", "cR1dlyUUJKp0s/Bel25u5TgvC3E="),
            (&[b'a'; 19], "yzaGyu0mcUukN7CdsSwa30tnCpc="),
            (&[b'a'; 20], "dUYRM7bOMwDmbriNIPr11x+r3E0="),
            (b"dGhlIHNhbXBsZSBub25jZQ==", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            (&[b'a'; 32], "4dFARmauxJvKNqg//3NO2jF7P/Y="),
        ];
        for (key, accept) in cases {
            assert_eq!(hash_key(key).unwrap(), accept, "key length {}", key.len());
        }
    }

    /// STATEMENT: Numeric close codes map to named or custom variants and reject reserved wire values.
    ///
    /// HOW:
    /// - Convert standard and application codes, comparing variants.
    /// - Check reserved and out-of-range values fail validity checks.
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

    /// STATEMENT: Close-code variants retain their protocol numbers and custom codes remain subject to validity rules.
    ///
    /// HOW:
    /// - Convert all named variants and an application code, then check an invalid custom code is rejected.
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

    /// STATEMENT: Challenge hashing enforces its key-length boundary.
    ///
    /// HOW:
    /// - Hash keys at and immediately above the maximum length and compare success with the bad-key error.
    #[test]
    fn test_hash_key_too_long() {
        assert_eq!(hash_key(&[b'a'; 33]), Err(HandshakeError::BadWebsocketKey));
        assert!(hash_key(&[b'a'; 32]).is_ok());
    }
}
