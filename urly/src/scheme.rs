use std::hash::{Hash, Hasher};

use crate::chars::SCHEME;
use crate::error::{ErrorKind, InvalidUrl};

/// URL scheme, e.g. `https`.
///
/// Comparison and hashing are ASCII case-insensitive. Schemes of a parsed
/// [`Url`](crate::Url) are always lowercase.
#[repr(transparent)]
pub struct Scheme(str);

str_type!(Scheme);

impl Scheme {
    /// `http`
    pub const HTTP: &'static Scheme = Scheme::from_str_unchecked("http");
    /// `https`
    pub const HTTPS: &'static Scheme = Scheme::from_str_unchecked("https");
    /// `ws`
    pub const WS: &'static Scheme = Scheme::from_str_unchecked("ws");
    /// `wss`
    pub const WSS: &'static Scheme = Scheme::from_str_unchecked("wss");
    /// `amqp`
    pub const AMQP: &'static Scheme = Scheme::from_str_unchecked("amqp");
    /// `amqps`
    pub const AMQPS: &'static Scheme = Scheme::from_str_unchecked("amqps");
    /// `mqtt`
    pub const MQTT: &'static Scheme = Scheme::from_str_unchecked("mqtt");
    /// `mqtts`
    pub const MQTTS: &'static Scheme = Scheme::from_str_unchecked("mqtts");

    /// Validates a scheme: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
    ///
    /// ```
    /// use urly::{ErrorKind, Scheme};
    ///
    /// assert_eq!(Scheme::new("HTTPS").unwrap(), Scheme::HTTPS);
    /// assert_eq!(Scheme::new("1http").unwrap_err().kind(), ErrorKind::InvalidScheme);
    /// ```
    pub fn new(src: &str) -> Result<&Scheme, InvalidUrl> {
        let bytes = src.as_bytes();
        match bytes.first() {
            None => Err(InvalidUrl::at(ErrorKind::Empty, 0)),
            Some(b) if !b.is_ascii_alphabetic() => Err(InvalidUrl::at(ErrorKind::InvalidScheme, 0)),
            _ => match bytes.iter().position(|b| !SCHEME.contains(*b)) {
                Some(i) => Err(InvalidUrl::at(ErrorKind::InvalidScheme, i)),
                None => Ok(Scheme::from_str_unchecked(src)),
            },
        }
    }

    /// Returns the default port of well-known schemes.
    ///
    /// ```
    /// use urly::Scheme;
    ///
    /// assert_eq!(Scheme::HTTPS.default_port(), Some(443));
    /// assert_eq!(Scheme::MQTTS.default_port(), Some(8883));
    /// assert_eq!(Scheme::new("foo").unwrap().default_port(), None);
    /// ```
    pub fn default_port(&self) -> Option<u16> {
        self.known().map(|(_, port, _)| port)
    }

    /// Returns `true` for WHATWG special schemes, whose empty path is
    /// normalized to `/`.
    pub(crate) fn is_special(&self) -> bool {
        self.known().is_some_and(|(_, _, special)| special)
    }

    fn known(&self) -> Option<(&'static str, u16, bool)> {
        const KNOWN: [(&str, u16, bool); 9] = [
            ("http", 80, true),
            ("https", 443, true),
            ("ws", 80, true),
            ("wss", 443, true),
            ("ftp", 21, true),
            ("amqp", 5672, false),
            ("amqps", 5671, false),
            ("mqtt", 1883, false),
            ("mqtts", 8883, false),
        ];
        KNOWN
            .into_iter()
            .find(|(s, _, _)| s.eq_ignore_ascii_case(&self.0))
    }
}

impl PartialEq for Scheme {
    fn eq(&self, other: &Scheme) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for Scheme {}

impl PartialEq<str> for Scheme {
    fn eq(&self, other: &str) -> bool {
        self.0.eq_ignore_ascii_case(other)
    }
}

impl PartialEq<Scheme> for str {
    fn eq(&self, other: &Scheme) -> bool {
        self.eq_ignore_ascii_case(&other.0)
    }
}

impl Hash for Scheme {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for b in self.0.bytes() {
            state.write_u8(b.to_ascii_lowercase());
        }
        state.write_u8(0xff);
    }
}
