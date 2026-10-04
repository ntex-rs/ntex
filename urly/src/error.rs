use std::{error::Error, fmt};

/// The reason a URL or URL component is invalid.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The input is empty.
    Empty,
    /// The URL is longer than 65535 bytes.
    TooLong,
    /// The scheme contains invalid characters.
    InvalidScheme,
    /// The URL has no scheme, but the operation requires one.
    SchemeMissing,
    /// The character is not allowed at this position.
    InvalidChar(char),
    /// A `%` is not followed by two hex digits.
    InvalidPercentEncoding,
    /// The authority is malformed.
    InvalidAuthority,
    /// The URL has no authority, but the operation requires one.
    AuthorityMissing,
    /// The host is malformed.
    InvalidHost,
    /// The host looks like an IPv4 address but isn't a valid one.
    InvalidIpv4,
    /// The bracketed host isn't a valid IPv6 address.
    InvalidIpv6,
    /// The port contains non-digit characters.
    InvalidPort,
    /// The port is greater than 65535.
    PortOutOfRange,
    /// The path, file name or extension is malformed.
    InvalidPath,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::Empty => f.write_str("empty string"),
            ErrorKind::TooLong => f.write_str("url is too long"),
            ErrorKind::InvalidScheme => f.write_str("invalid scheme"),
            ErrorKind::SchemeMissing => f.write_str("scheme missing"),
            ErrorKind::InvalidChar(c) => write!(f, "invalid character {c:?}"),
            ErrorKind::InvalidPercentEncoding => f.write_str("invalid percent-encoding"),
            ErrorKind::InvalidAuthority => f.write_str("invalid authority"),
            ErrorKind::AuthorityMissing => f.write_str("authority missing"),
            ErrorKind::InvalidHost => f.write_str("invalid host"),
            ErrorKind::InvalidIpv4 => f.write_str("invalid IPv4 address"),
            ErrorKind::InvalidIpv6 => f.write_str("invalid IPv6 address"),
            ErrorKind::InvalidPort => f.write_str("invalid port"),
            ErrorKind::PortOutOfRange => f.write_str("port out of range"),
            ErrorKind::InvalidPath => f.write_str("invalid path"),
        }
    }
}

/// An error returned when a URL or URL component is invalid.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct InvalidUrl {
    kind: ErrorKind,
    position: Option<usize>,
}

impl InvalidUrl {
    pub(crate) const fn new(kind: ErrorKind) -> Self {
        InvalidUrl {
            kind,
            position: None,
        }
    }

    pub(crate) const fn at(kind: ErrorKind, position: usize) -> Self {
        InvalidUrl {
            kind,
            position: Some(position),
        }
    }

    /// Shifts the error position by `offset` bytes.
    pub(crate) fn offset(mut self, offset: usize) -> Self {
        if let Some(pos) = self.position.as_mut() {
            *pos += offset;
        }
        self
    }

    /// Returns the reason the input is invalid.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the byte offset of the error in the input, if known.
    pub fn position(&self) -> Option<usize> {
        self.position
    }
}

impl fmt::Display for InvalidUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(pos) = self.position {
            write!(f, "{} at position {pos}", self.kind)
        } else {
            fmt::Display::fmt(&self.kind, f)
        }
    }
}

impl Error for InvalidUrl {}

/// An error returned by [`Url::from_parts`](crate::Url::from_parts).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct InvalidUrlParts(InvalidUrl);

impl InvalidUrlParts {
    /// Returns the reason the parts are invalid.
    pub fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    /// Returns the underlying error.
    pub fn into_inner(self) -> InvalidUrl {
        self.0
    }
}

impl From<InvalidUrl> for InvalidUrlParts {
    fn from(err: InvalidUrl) -> Self {
        InvalidUrlParts(err)
    }
}

impl fmt::Display for InvalidUrlParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid url parts: {}", self.0)
    }
}

impl Error for InvalidUrlParts {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}
