//! Lenient parsing with yarl-style normalization, and strict RFC 3986 validation.

use std::borrow::Cow;

use ntex_bytes::ByteString;

use crate::authority::{self, offset, parse_port, validate_authority};
use crate::chars::{self, PATH, QUERY, lowercase, scheme_len, split_at_char};
use crate::error::{ErrorKind, InvalidUrl};
use crate::host::normalize_host;
use crate::quoting::{Component, requote};
use crate::scheme::Scheme;
use crate::url::{Components, Url, assemble};

/// Maximum URL length in bytes.
pub(crate) const MAX_LEN: usize = u16::MAX as usize;

/// Splits a URL reference into its raw components.
fn split(s: &str) -> Components<'_> {
    let (scheme, rest) = match scheme_len(s) {
        Some(n) => (Some(&s[..n]), &s[n + 1..]),
        None => (None, s),
    };
    let (rest, fragment) = split_at_char(rest, '#');
    let (rest, query) = split_at_char(rest, '?');
    let (authority, path) = match rest.strip_prefix("//") {
        Some(r) => {
            let (authority, path) = r.split_at(r.find('/').unwrap_or(r.len()));
            (Some(authority), path)
        }
        None => (None, rest),
    };
    Components {
        scheme,
        authority,
        path,
        query,
        fragment,
    }
}

/// Parses and normalizes a URL reference.
///
/// `orig` is the buffer `src` came from; it is reused if no normalization is needed.
pub(crate) fn parse(src: &str, orig: Option<&ByteString>) -> Result<Url, InvalidUrl> {
    let (cleaned, lead) = clean(src)?;
    parse_clean(&cleaned, orig).map_err(|e| e.offset(lead))
}

/// Parses and normalizes an authority-form `[userinfo@]host[:port]`.
pub(crate) fn parse_authority(src: &str) -> Result<Url, InvalidUrl> {
    let (cleaned, lead) = clean(src)?;
    parse_authority_clean(&cleaned).map_err(|e| e.offset(lead))
}

fn parse_authority_clean(s: &str) -> Result<Url, InvalidUrl> {
    if let Some(i) = s.find(['/', '?', '#']) {
        return Err(InvalidUrl::at(
            ErrorKind::InvalidChar(char::from(s.as_bytes()[i])),
            i,
        ));
    }
    let host = authority::split(s).1;
    if host.is_empty() {
        return Err(InvalidUrl::at(ErrorKind::InvalidHost, offset(s, host)));
    }
    let authority = normalize_authority(s)?;
    assemble(
        &Components {
            authority: Some(&authority),
            ..Components::default()
        },
        None,
    )
}

/// Trims whitespace and removes tabs and newlines, returns the cleaned input
/// and the number of leading bytes removed.
fn clean(src: &str) -> Result<(Cow<'_, str>, usize), InvalidUrl> {
    let trimmed = src.trim_start_matches(|c: char| c <= ' ');
    let lead = src.len() - trimmed.len();
    let trimmed = trimmed.trim_end_matches(|c: char| c <= ' ');
    if trimmed.is_empty() {
        return Err(InvalidUrl::new(ErrorKind::Empty));
    }
    if trimmed.len() > MAX_LEN {
        return Err(InvalidUrl::new(ErrorKind::TooLong));
    }
    // branchless prefilter vectorizes, `\t`, `\n` and `\r` are below 0x0e
    let cleaned = if trimmed.bytes().fold(false, |a, b| a | (b < 0x0e))
        && trimmed.contains(['\t', '\n', '\r'])
    {
        Cow::Owned(trimmed.replace(['\t', '\n', '\r'], ""))
    } else {
        Cow::Borrowed(trimmed)
    };
    Ok((cleaned, lead))
}

fn parse_clean(s: &str, orig: Option<&ByteString>) -> Result<Url, InvalidUrl> {
    let c = split(s);
    let scheme = c.scheme.map(|s| lowercase(s.into()));
    let authority = c
        .authority
        .map(|a| normalize_authority(a).map_err(|e| e.offset(offset(s, a))))
        .transpose()?;
    let path = normalize_path(c.path);
    let query = c.query.map(|q| requote(q, Component::Query));
    let fragment = c.fragment.map(|f| requote(f, Component::Fragment));

    assemble(
        &Components {
            scheme: scheme.as_deref(),
            authority: authority.as_deref(),
            path: &path,
            query: query.as_deref(),
            fragment: fragment.as_deref(),
        },
        orig,
    )
}

/// Requotes a path and removes dot segments from absolute paths.
pub(crate) fn normalize_path(path: &str) -> Cow<'_, str> {
    let path = requote(path, Component::Path);
    if path.starts_with('/') && has_dot_segments(&path) {
        Cow::Owned(remove_dot_segments(&path))
    } else {
        path
    }
}

/// Normalizes an authority: requotes userinfo, normalizes the host and port.
pub(crate) fn normalize_authority(a: &str) -> Result<Cow<'_, str>, InvalidUrl> {
    let (userinfo, host, port) = authority::split(a);
    let host_pos = offset(a, host);
    let new_host = normalize_host(host).map_err(|e| e.offset(host_pos))?;

    let mut changed = matches!(new_host, Cow::Owned(_));
    let new_port = match port {
        None => None,
        Some("") => {
            changed = true;
            None
        }
        Some(p) => {
            let port = parse_port(p).map_err(|e| e.offset(offset(a, p)))?;
            changed |= p.len() > 1 && p.starts_with('0');
            Some(port)
        }
    };
    if new_host.is_empty() && (userinfo.is_some() || new_port.is_some()) {
        return Err(InvalidUrl::at(ErrorKind::InvalidHost, host_pos));
    }
    let new_userinfo = userinfo.map(|u| match u.split_once(':') {
        Some((user, password)) => {
            let user = requote(user, Component::UserInfo);
            let password = requote(password, Component::UserInfo);
            if matches!((&user, &password), (Cow::Borrowed(_), Cow::Borrowed(_))) {
                Cow::Borrowed(u)
            } else {
                Cow::Owned(format!("{user}:{password}"))
            }
        }
        None => requote(u, Component::UserInfo),
    });
    changed |= matches!(new_userinfo, Some(Cow::Owned(_)));

    if !changed {
        return Ok(Cow::Borrowed(a));
    }
    Ok(Cow::Owned(join_authority(
        new_userinfo.as_deref(),
        &new_host,
        new_port,
    )))
}

pub(crate) fn join_authority(userinfo: Option<&str>, host: &str, port: Option<u16>) -> String {
    let mut out = String::with_capacity(host.len() + 16);
    if let Some(userinfo) = userinfo {
        out.push_str(userinfo);
        out.push('@');
    }
    out.push_str(host);
    if let Some(port) = port {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out
}

pub(crate) fn has_dot_segments(path: &str) -> bool {
    let b = path.as_bytes();
    b.iter().enumerate().any(|(i, &c)| {
        c == b'.' && (i == 0 || b[i - 1] == b'/') && {
            let end = if b.get(i + 1) == Some(&b'.') { i + 2 } else { i + 1 };
            end == b.len() || b[end] == b'/'
        }
    })
}

/// RFC 3986 section 5.2.4.
pub(crate) fn remove_dot_segments(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let rest = match path.strip_prefix('/') {
        Some(rest) => {
            result.push('/');
            rest
        }
        None => path,
    };
    // output segments are joined with `/` after `base`
    let base = result.len();
    let mut count = 0usize;
    let mut trailing_slash = false;
    for segment in rest.split('/') {
        match segment {
            "." => trailing_slash = true,
            ".." => {
                if count > 0 {
                    let pos = result[base..].rfind('/').map_or(base, |i| base + i);
                    result.truncate(pos);
                    count -= 1;
                }
                trailing_slash = true;
            }
            s => {
                if count > 0 {
                    result.push('/');
                }
                result.push_str(s);
                count += 1;
                trailing_slash = false;
            }
        }
    }
    if trailing_slash && count > 0 {
        result.push('/');
    }
    result
}

/// Strictly validates a URI reference.
pub(crate) fn validate(src: &str) -> Result<(), InvalidUrl> {
    if src.is_empty() {
        return Err(InvalidUrl::new(ErrorKind::Empty));
    }
    if src.len() > MAX_LEN {
        return Err(InvalidUrl::new(ErrorKind::TooLong));
    }
    // a colon in the first segment can only be a scheme delimiter
    if scheme_len(src).is_none()
        && let Some(i) = src.find([':', '/', '?', '#'])
        && src.as_bytes()[i] == b':'
    {
        return Err(match Scheme::new(&src[..i]) {
            Err(e) if e.kind() == ErrorKind::InvalidScheme => e,
            _ => InvalidUrl::at(ErrorKind::InvalidScheme, 0),
        });
    }
    let c = split(src);
    if let Some(a) = c.authority {
        validate_authority(a).map_err(|e| e.offset(offset(src, a)))?;
    }
    for (part, allowed) in [
        (Some(c.path), &PATH),
        (c.query, &QUERY),
        (c.fragment, &QUERY),
    ] {
        if let Some(part) = part {
            chars::check(part, allowed).map_err(|e| e.offset(offset(src, part)))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_segments() {
        let cases = [
            ("/a/b/c/./../../g", "/a/g"),
            ("/a/b/..", "/a/"),
            ("/..", "/"),
            ("/.", "/"),
            ("/a/./", "/a/"),
            ("/a/../../b", "/b"),
            ("../a", "a"),
            (".", ""),
            ("/a//../b", "/a/b"),
            ("//.", "//"),
            ("//..", "/"),
            ("a/b/../../c", "c"),
            ("/a/b/../..", "/"),
            ("/a/.b/../c", "/a/c"),
        ];
        for (input, expected) in cases {
            assert_eq!(remove_dot_segments(input), expected, "{input}");
        }
        assert!(!has_dot_segments("/a/.b/c./..b/"));
        for path in [
            ".", "..", "/.", "/..", "./a", "../a", "/a/./b", "/a/../b", "a/.",
        ] {
            assert!(has_dot_segments(path), "{path}");
        }
    }

    #[test]
    fn authority() {
        assert!(matches!(
            normalize_authority("u:p@a.com:80"),
            Ok(Cow::Borrowed(_))
        ));
        assert_eq!(normalize_authority("U@A.com:").unwrap(), "U@a.com");
        assert_eq!(normalize_authority("a b:c@h:080").unwrap(), "a%20b:c@h:80");
        assert_eq!(normalize_authority("a@b@h").unwrap(), "a%40b@h");
        let err = normalize_authority("h:8a").unwrap_err();
        assert_eq!(
            (err.kind(), err.position()),
            (ErrorKind::InvalidPort, Some(3))
        );
        let err = normalize_authority("h:65536").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PortOutOfRange);
        let err = normalize_authority("u@:80").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidHost);
    }

    #[test]
    fn authority_form() {
        for (src, expected) in [
            ("custom.domain", "//custom.domain"),
            (" Custom.Domain:080 ", "//custom.domain:80"),
            ("u:p@h:1", "//u:p@h:1"),
            ("[::1]:8080", "//[::1]:8080"),
            ("h:", "//h"),
        ] {
            assert_eq!(parse_authority(src).unwrap(), expected, "{src}");
        }
        let cases = [
            ("", ErrorKind::Empty, None),
            ("h/p", ErrorKind::InvalidChar('/'), Some(1)),
            (" h?q", ErrorKind::InvalidChar('?'), Some(2)),
            ("h#f", ErrorKind::InvalidChar('#'), Some(1)),
            (":", ErrorKind::InvalidHost, Some(0)),
            ("u@:80", ErrorKind::InvalidHost, Some(2)),
            ("h:8a", ErrorKind::InvalidPort, Some(3)),
        ];
        for (input, kind, pos) in cases {
            let err = parse_authority(input).unwrap_err();
            assert_eq!((err.kind(), err.position()), (kind, pos), "{input}");
        }
    }

    #[test]
    fn strict() {
        for valid in [
            "http://u:p@example.com:8080/a/b?c=d&e#f",
            "//example.com",
            "/a:b",
            "a/b:c",
            "?q",
            "#f",
            "mailto:a@b.c",
            "urn:isbn:0451450523",
            "http://[::1]/",
            "file:///etc/hosts",
        ] {
            assert_eq!(validate(valid), Ok(()), "{valid}");
        }
        let cases = [
            ("", ErrorKind::Empty, None),
            ("http://ex ample.com", ErrorKind::InvalidChar(' '), Some(9)),
            ("1http://a", ErrorKind::InvalidScheme, Some(0)),
            ("ht_tp://a", ErrorKind::InvalidScheme, Some(2)),
            (":a", ErrorKind::InvalidScheme, Some(0)),
            ("http://a/%zz", ErrorKind::InvalidPercentEncoding, Some(9)),
            ("http://a/?x#y#", ErrorKind::InvalidChar('#'), Some(13)),
            ("http://a:1x/", ErrorKind::InvalidPort, Some(10)),
            ("http://a:70000", ErrorKind::PortOutOfRange, Some(9)),
            ("http://[::1/", ErrorKind::InvalidIpv6, Some(7)),
            ("/ü", ErrorKind::InvalidChar('ü'), Some(1)),
        ];
        for (input, kind, pos) in cases {
            let err = validate(input).unwrap_err();
            assert_eq!((err.kind(), err.position()), (kind, pos), "{input}");
        }
    }
}
