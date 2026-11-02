//! Client certificate authentication checks.
//!
//! Policy-free building blocks used by the broker: name matching against the subject CN or a
//! SAN kind, thumbprint allow-lists and validity period checks. Trust (chain) verification is
//! in [`crate::x509::trust`]. Every failure is a distinct [`Failure`] so callers can map it to their
//! own error codes.
//!
//! Decisions:
//! - Name comparison is case-insensitive (Unicode lowercase of both sides).
//! - Subject matching uses the **first** CN attribute only.
//! - SAN matching succeeds if **any** entry of the kind matches.
//! - IP matching compares parsed addresses (`::0:1` equals `::1`); SAN IP entries that are
//!   not 4 or 16 octets are ignored when matching, but rejected by [`field`].
//! - Thumbprints are hex, compared case-insensitively; exactly 40 characters selects SHA-1,
//!   anything else SHA-256 (see [`crate::x509::digest::Algorithm::for_hex_thumbprint`]).
use std::net::IpAddr;
use std::time::SystemTime;

use crate::x509::{Certificate, digest::Algorithm};

/// Certificate field used as the client name.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum NameField {
    /// First subject common name.
    Subject,
    /// SAN `dNSName`.
    Dns,
    /// SAN `uniformResourceIdentifier`.
    Uri,
    /// SAN `iPAddress`.
    Ip,
    /// SAN `rfc822Name`.
    Email,
}

/// Authentication check failure.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Failure {
    /// Client name does not match the certificate field.
    Mismatch(NameField),
    /// Client name is not an IP address (for [`NameField::Ip`]).
    InvalidClientIp,
    /// More than one SAN entry of the kind (see [`field`]).
    MultipleSan(NameField),
    /// SAN `iPAddress` entry is neither 4 nor 16 octets.
    MalformedSanIp,
    /// Current time is before `notBefore`.
    NotYetValid,
    /// Current time is after `notAfter`.
    Expired,
    /// No allowed thumbprint matches the certificate.
    ThumbprintMismatch,
}

/// Checks that `name` matches the certificate `field`.
///
/// # Errors
/// [`Failure::Mismatch`], or [`Failure::InvalidClientIp`] if `field` is [`NameField::Ip`] and
/// `name` is not an IP address.
pub fn name_matches(cert: &Certificate, field: NameField, name: &str) -> Result<(), Failure> {
    let eq = |v: &str| v.to_lowercase() == name.to_lowercase();
    let san = cert.san();
    let matched = match field {
        NameField::Subject => cert.subject_cn().is_some_and(eq),
        NameField::Dns => san.dns.iter().any(|v| eq(v)),
        NameField::Uri => san.uri.iter().any(|v| eq(v)),
        NameField::Email => san.email.iter().any(|v| eq(v)),
        NameField::Ip => {
            let ip: IpAddr = name.parse().map_err(|_| Failure::InvalidClientIp)?;
            san.ip.iter().any(|v| v.addr() == Some(ip))
        }
    };
    if matched {
        Ok(())
    } else {
        Err(Failure::Mismatch(field))
    }
}

/// Returns the client name stored in the certificate `field`, lowercased.
///
/// `None` if the field is absent. IP addresses are formatted with [`IpAddr`]'s `Display`.
///
/// # Errors
/// [`Failure::MultipleSan`] if there is more than one SAN entry of the kind,
/// [`Failure::MalformedSanIp`] for an invalid `iPAddress` entry.
pub fn field(cert: &Certificate, field: NameField) -> Result<Option<String>, Failure> {
    fn single<T>(v: &[T], field: NameField) -> Result<Option<&T>, Failure> {
        match v {
            [] => Ok(None),
            [v] => Ok(Some(v)),
            _ => Err(Failure::MultipleSan(field)),
        }
    }
    let san = cert.san();
    Ok(match field {
        NameField::Subject => cert.subject_cn().map(str::to_lowercase),
        NameField::Dns => single(&san.dns, field)?.map(|v| v.to_lowercase()),
        NameField::Uri => single(&san.uri, field)?.map(|v| v.to_lowercase()),
        NameField::Email => single(&san.email, field)?.map(|v| v.to_lowercase()),
        NameField::Ip => match single(&san.ip, field)? {
            Some(ip) => Some(ip.addr().ok_or(Failure::MalformedSanIp)?.to_string()),
            None => None,
        },
    })
}

/// Checks that `now` is within the certificate validity period (bounds inclusive) and
/// returns `notAfter`.
///
/// # Errors
/// [`Failure::NotYetValid`] or [`Failure::Expired`].
pub fn validity(cert: &Certificate, now: SystemTime) -> Result<SystemTime, Failure> {
    if now < cert.not_before() {
        Err(Failure::NotYetValid)
    } else if now > cert.not_after() {
        Err(Failure::Expired)
    } else {
        Ok(cert.not_after())
    }
}

/// Returns `true` if any of the hex `thumbprints` matches the certificate.
pub fn thumbprint_matches<'a>(
    cert: &Certificate,
    thumbprints: impl IntoIterator<Item = &'a str>,
) -> bool {
    thumbprints.into_iter().any(|t| {
        let alg = Algorithm::for_hex_thumbprint(t);
        hex::decode(t).is_ok_and(|t| t.as_slice() == cert.digest(alg))
    })
}

/// Checks validity period and thumbprint allow-list, returns `notAfter`.
///
/// # Errors
/// [`Failure::NotYetValid`], [`Failure::Expired`] or [`Failure::ThumbprintMismatch`].
pub fn verify_thumbprint<'a>(
    cert: &Certificate,
    thumbprints: impl IntoIterator<Item = &'a str>,
    now: SystemTime,
) -> Result<SystemTime, Failure> {
    let expires = validity(cert, now)?;
    if thumbprint_matches(cert, thumbprints) {
        Ok(expires)
    } else {
        Err(Failure::ThumbprintMismatch)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::x509::testgen::{self, San};

    fn cert(san: &San<'_>) -> Certificate {
        testgen::cert("Client1", false, None, &testgen::key(), san, (-1, 1))
    }

    /// STATEMENT: Name matching normalizes case and addresses but rejects ambiguous SAN fields.
    ///
    /// HOW:
    /// - Generate subject-only and multi-SAN certificates and check matches and field errors.
    #[test]
    fn names() {
        let c = cert(&San::default());
        assert_eq!(name_matches(&c, NameField::Subject, "CLIENT1"), Ok(()));
        assert_eq!(
            name_matches(&c, NameField::Dns, "client1"),
            Err(Failure::Mismatch(NameField::Dns))
        );
        assert_eq!(
            name_matches(&c, NameField::Ip, "x"),
            Err(Failure::InvalidClientIp)
        );
        assert_eq!(field(&c, NameField::Subject), Ok(Some("client1".into())));
        assert_eq!(field(&c, NameField::Email), Ok(None));

        let c = cert(&San {
            dns: &["A.b", "c.d"],
            email: &["X@y"],
            ip: &["::1".parse().unwrap()],
            ..San::default()
        });
        assert_eq!(name_matches(&c, NameField::Dns, "c.D"), Ok(()));
        assert_eq!(name_matches(&c, NameField::Ip, "::0:1"), Ok(()));
        assert_eq!(
            field(&c, NameField::Dns),
            Err(Failure::MultipleSan(NameField::Dns))
        );
        assert_eq!(field(&c, NameField::Ip), Ok(Some("::1".into())));
        assert_eq!(field(&c, NameField::Email), Ok(Some("x@y".into())));
    }

    /// STATEMENT: Thumbprint authorization requires both a matching digest and valid dates.
    ///
    /// HOW:
    /// - Check both hash algorithms, malformed/mismatched values, and times outside validity.
    #[test]
    fn thumbprints_and_validity() {
        let c = cert(&San::default());
        let now = SystemTime::now();
        let sha1 = c.thumbprint(Algorithm::Sha1).to_uppercase();
        let sha256 = c.thumbprint(Algorithm::Sha256);
        assert_eq!(
            verify_thumbprint(&c, [sha1.as_str()], now),
            Ok(c.not_after())
        );
        assert_eq!(
            verify_thumbprint(&c, ["zz", sha256.as_str()], now),
            Ok(c.not_after())
        );
        assert_eq!(
            verify_thumbprint(&c, [&sha256[2..]], now),
            Err(Failure::ThumbprintMismatch)
        );
        let day = Duration::from_hours(48);
        assert_eq!(
            verify_thumbprint(&c, [sha1.as_str()], now - day),
            Err(Failure::NotYetValid)
        );
        assert_eq!(
            verify_thumbprint(&c, [sha1.as_str()], now + day),
            Err(Failure::Expired)
        );
    }
}
