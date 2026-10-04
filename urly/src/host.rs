use std::borrow::Cow;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::chars::{self, NONE, REG_NAME, char_at, lowercase};
use crate::error::{ErrorKind, InvalidUrl};
use crate::idna;
use crate::quoting::unquote_with;

/// Parsed URL host.
///
/// ```
/// use std::net::Ipv6Addr;
/// use urly::Host;
///
/// assert_eq!(Host::parse("example.com").unwrap(), Host::Domain("example.com"));
/// assert_eq!(Host::parse("[::1]").unwrap(), Host::Ipv6(Ipv6Addr::LOCALHOST));
/// assert!(Host::parse("exa mple.com").is_err());
/// ```
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Host<'a> {
    /// Registered name
    Domain(&'a str),
    /// IPv4 address
    Ipv4(Ipv4Addr),
    /// IPv6 address, written in brackets
    Ipv6(Ipv6Addr),
}

impl<'a> Host<'a> {
    /// Strictly validates and classifies a host.
    pub fn parse(src: &'a str) -> Result<Host<'a>, InvalidUrl> {
        validate_host(src)?;
        Ok(Host::classify(src))
    }

    pub(crate) fn classify(src: &'a str) -> Host<'a> {
        if let Some(inner) = src.strip_prefix('[')
            && let Ok(addr) = ipv6(inner)
        {
            Host::Ipv6(addr)
        } else if looks_like_ipv4(src)
            && let Ok(addr) = src.parse()
        {
            Host::Ipv4(addr)
        } else {
            Host::Domain(src)
        }
    }

    /// Returns the host with punycode-encoded labels decoded.
    ///
    /// ```
    /// use urly::Host;
    ///
    /// let host = Host::parse("xn--mnchen-3ya.de").unwrap();
    /// assert_eq!(host.to_unicode(), "münchen.de");
    /// ```
    pub fn to_unicode(&self) -> Cow<'a, str> {
        match self {
            Host::Domain(s) => idna::domain_to_unicode(s),
            host => Cow::Owned(host.to_string()),
        }
    }
}

impl fmt::Display for Host<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Host::Domain(s) => f.write_str(s),
            Host::Ipv4(addr) => fmt::Display::fmt(addr, f),
            Host::Ipv6(addr) => write!(f, "[{addr}]"),
        }
    }
}

fn looks_like_ipv4(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && s.bytes().filter(|b| *b == b'.').count() == 3
}

/// Parses the part of an IP literal after `[`.
fn ipv6(inner: &str) -> Result<Ipv6Addr, InvalidUrl> {
    inner
        .strip_suffix(']')
        .and_then(|inner| inner.parse().ok())
        .ok_or(InvalidUrl::at(ErrorKind::InvalidIpv6, 0))
}

/// Rejects dotted-decimal hosts that are not valid IPv4 addresses.
fn check_ipv4(s: &str) -> Result<(), InvalidUrl> {
    if looks_like_ipv4(s) && s.parse::<Ipv4Addr>().is_err() {
        Err(InvalidUrl::at(ErrorKind::InvalidIpv4, 0))
    } else {
        Ok(())
    }
}

pub(crate) fn validate_host(s: &str) -> Result<(), InvalidUrl> {
    if let Some(inner) = s.strip_prefix('[') {
        return ipv6(inner).map(|_| ());
    }
    chars::check(s, &REG_NAME)?;
    check_ipv4(s)
}

/// Leniently normalizes a host: decodes escapes, lowercases, punycode-encodes
/// non-ASCII labels, canonicalizes IPv6.
pub(crate) fn normalize_host(s: &str) -> Result<Cow<'_, str>, InvalidUrl> {
    if let Some(inner) = s.strip_prefix('[') {
        let canonical = format!("[{}]", ipv6(inner)?);
        return Ok(if canonical == s {
            Cow::Borrowed(s)
        } else {
            Cow::Owned(canonical)
        });
    }

    let decoded = unquote_with(s, false, &NONE);
    let host = if !decoded.is_ascii() {
        Cow::Owned(
            idna::domain_to_ascii(&decoded).ok_or(InvalidUrl::at(ErrorKind::InvalidHost, 0))?,
        )
    } else {
        lowercase(decoded)
    };
    if let Some(i) = host.bytes().position(|b| !REG_NAME.contains(b)) {
        // positions are only meaningful if nothing was decoded
        return Err(if s.len() == host.len() && s.is_ascii() {
            InvalidUrl::at(ErrorKind::InvalidChar(char_at(&host, i)), i)
        } else {
            InvalidUrl::at(ErrorKind::InvalidHost, 0)
        });
    }
    check_ipv4(&host)?;
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize() {
        assert!(matches!(
            normalize_host("a.com"),
            Ok(Cow::Borrowed("a.com"))
        ));
        assert_eq!(normalize_host("A.Com").unwrap(), "a.com");
        assert_eq!(normalize_host("%41.com").unwrap(), "a.com");
        assert_eq!(normalize_host("[0:0::1]").unwrap(), "[::1]");
        assert_eq!(
            normalize_host("a b").unwrap_err().kind(),
            ErrorKind::InvalidChar(' ')
        );
        assert_eq!(
            normalize_host("a%20b").unwrap_err().kind(),
            ErrorKind::InvalidHost
        );
        assert_eq!(normalize_host("München.de").unwrap(), "xn--mnchen-3ya.de");
        assert_eq!(
            normalize_host("m%C3%BCnchen.de").unwrap(),
            "xn--mnchen-3ya.de"
        );
        assert_eq!(
            normalize_host("mü nchen.de").unwrap_err().kind(),
            ErrorKind::InvalidHost
        );
        assert_eq!(
            normalize_host("1.2.3.999").unwrap_err().kind(),
            ErrorKind::InvalidIpv4
        );
        assert_eq!(
            normalize_host("[::g]").unwrap_err().kind(),
            ErrorKind::InvalidIpv6
        );
    }

    #[test]
    fn classify() {
        assert_eq!(Host::classify("127.0.0.1"), Host::Ipv4(Ipv4Addr::LOCALHOST));
        assert_eq!(Host::classify("1.2.3"), Host::Domain("1.2.3"));
        assert_eq!(Host::Ipv6(Ipv6Addr::LOCALHOST).to_string(), "[::1]");
    }
}
