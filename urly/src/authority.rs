use std::{borrow::Cow, fmt};

use crate::error::{ErrorKind, InvalidUrl};
use crate::quoting::{Component, unquote};
use crate::{chars, host::validate_host};

/// URL authority: `[userinfo "@"] host [":" port]`.
#[repr(transparent)]
pub struct Authority(str);

str_type!(Authority);
str_eq!(Authority);

impl Authority {
    /// Strictly validates an authority.
    ///
    /// ```
    /// use urly::{Authority, ErrorKind};
    ///
    /// let auth = Authority::new("user:pw@example.com:8080").unwrap();
    /// assert_eq!(auth.host(), "example.com");
    /// assert_eq!(auth.port_u16(), Some(8080));
    /// assert_eq!(auth.userinfo().unwrap().password(), Some("pw"));
    ///
    /// let err = Authority::new("example.com:99999").unwrap_err();
    /// assert_eq!(err.kind(), ErrorKind::PortOutOfRange);
    /// ```
    pub fn new(src: &str) -> Result<&Authority, InvalidUrl> {
        validate_authority(src)?;
        Ok(Authority::from_str_unchecked(src))
    }

    /// Converts a static string to an authority.
    ///
    /// # Panics
    ///
    /// Panics if the authority is not valid.
    pub fn from_static(src: &'static str) -> &'static Authority {
        match Authority::new(src) {
            Ok(auth) => auth,
            Err(e) => panic!("invalid static authority {src:?}: {e}"),
        }
    }

    /// Returns the userinfo, if present.
    pub fn userinfo(&self) -> Option<&UserInfo> {
        split(&self.0).0.map(UserInfo::from_str_unchecked)
    }

    /// Returns the host. IPv6 addresses include the brackets.
    pub fn host(&self) -> &str {
        split(&self.0).1
    }

    /// Returns the port, if present.
    pub fn port(&self) -> Option<Port<&str>> {
        split(&self.0).2.and_then(Port::parse)
    }

    /// Returns the port as a number, if present.
    pub fn port_u16(&self) -> Option<u16> {
        self.port().map(|p| p.as_u16())
    }

    /// Returns the authority without the userinfo.
    pub(crate) fn host_port(&self) -> &str {
        self.0.rsplit_once('@').map_or(&self.0, |(_, hp)| hp)
    }
}

/// Splits an authority into userinfo, host and port.
pub(crate) fn split(s: &str) -> (Option<&str>, &str, Option<&str>) {
    let (userinfo, hp) = match s.rfind('@') {
        Some(i) => (Some(&s[..i]), &s[i + 1..]),
        None => (None, s),
    };
    if hp.starts_with('[') {
        if let Some(end) = hp.find(']') {
            let rest = &hp[end + 1..];
            if rest.is_empty() {
                return (userinfo, hp, None);
            }
            if let Some(port) = rest.strip_prefix(':') {
                return (userinfo, &hp[..=end], Some(port));
            }
        }
        return (userinfo, hp, None);
    }
    match hp.rfind(':') {
        Some(i) => (userinfo, &hp[..i], Some(&hp[i + 1..])),
        None => (userinfo, hp, None),
    }
}

/// Byte offset of `inner` within `outer`.
pub(crate) fn offset(outer: &str, inner: &str) -> usize {
    inner.as_ptr() as usize - outer.as_ptr() as usize
}

pub(crate) fn validate_authority(s: &str) -> Result<(), InvalidUrl> {
    let (userinfo, host, port) = split(s);
    if let Some(userinfo) = userinfo {
        chars::check(userinfo, &chars::USERINFO)?;
    }
    validate_host(host).map_err(|e| e.offset(offset(s, host)))?;
    if let Some(port) = port {
        parse_port(port).map_err(|e| e.offset(offset(s, port)))?;
    }
    Ok(())
}

/// Parses a decimal port.
pub(crate) fn parse_port(s: &str) -> Result<u16, InvalidUrl> {
    if let Some(i) = s.bytes().position(|b| !b.is_ascii_digit()) {
        return Err(InvalidUrl::at(ErrorKind::InvalidPort, i));
    }
    s.bytes()
        .try_fold(0u16, |acc, b| {
            acc.checked_mul(10)?.checked_add(u16::from(b - b'0'))
        })
        .ok_or(InvalidUrl::at(ErrorKind::PortOutOfRange, 0))
}

/// URL userinfo: `user [":" password]`, percent-encoded.
#[repr(transparent)]
pub struct UserInfo(str);

str_type!(UserInfo);
str_eq!(UserInfo);

impl UserInfo {
    /// Strictly validates a userinfo.
    pub fn new(src: &str) -> Result<&UserInfo, InvalidUrl> {
        chars::check(src, &chars::USERINFO)?;
        Ok(UserInfo::from_str_unchecked(src))
    }

    /// Returns the percent-encoded user name.
    pub fn username(&self) -> &str {
        self.0.split_once(':').map_or(&self.0, |(user, _)| user)
    }

    /// Returns the percent-encoded password, if present.
    pub fn password(&self) -> Option<&str> {
        self.0.split_once(':').map(|(_, password)| password)
    }

    /// Returns the decoded user name.
    pub fn decoded_username(&self) -> Cow<'_, str> {
        unquote(self.username(), Component::UserInfo)
    }

    /// Returns the decoded password, if present.
    pub fn decoded_password(&self) -> Option<Cow<'_, str>> {
        self.password().map(|p| unquote(p, Component::UserInfo))
    }
}

/// URL port.
///
/// `T` is the textual representation the port was parsed from.
#[derive(Copy, Clone, Debug)]
pub struct Port<T> {
    port: u16,
    repr: T,
}

impl<'a> Port<&'a str> {
    pub(crate) fn parse(repr: &'a str) -> Option<Self> {
        let port = parse_port(repr).ok().filter(|_| !repr.is_empty())?;
        Some(Port { port, repr })
    }
}

impl<T> Port<T> {
    /// Returns the port number.
    pub const fn as_u16(&self) -> u16 {
        self.port
    }
}

impl<T: AsRef<str>> Port<T> {
    /// Returns the port as written in the URL.
    pub fn as_str(&self) -> &str {
        self.repr.as_ref()
    }
}

impl<T> PartialEq<u16> for Port<T> {
    fn eq(&self, other: &u16) -> bool {
        self.port == *other
    }
}

impl<T, U> PartialEq<Port<U>> for Port<T> {
    fn eq(&self, other: &Port<U>) -> bool {
        self.port == other.port
    }
}

impl<T: AsRef<str>> fmt::Display for Port<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_authority() {
        assert_eq!(split("a@b:1"), (Some("a"), "b", Some("1")));
        assert_eq!(split("a:p@b"), (Some("a:p"), "b", None));
        assert_eq!(split("[::1]:80"), (None, "[::1]", Some("80")));
        assert_eq!(split("[::1]"), (None, "[::1]", None));
        assert_eq!(split("[::1]x"), (None, "[::1]x", None));
        assert_eq!(split("b:"), (None, "b", Some("")));
    }

    #[test]
    fn strict() {
        assert!(Authority::new("").is_ok());
        assert!(Authority::new("u:p@h.com:").is_ok());
        assert!(Authority::new("[::1]:65535").is_ok());
        let err = Authority::new("u p@h").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidChar(' '));
        assert_eq!(err.position(), Some(1));
        let err = Authority::new("h:8x").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidPort);
        assert_eq!(err.position(), Some(3));
        let err = Authority::new("[::1").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidIpv6);
        let err = Authority::new("1.2.3.256").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidIpv4);
    }

    #[test]
    fn port() {
        let p = Port::parse("0080").unwrap();
        assert_eq!(p, 80);
        assert_eq!(p.as_str(), "0080");
        assert!(Port::parse("65536").is_none());
        assert!(Port::parse("").is_none());
        assert!(Port::parse("1a").is_none());
    }
}
