use std::{borrow::ToOwned, cell::Ref};

use super::config::WebAppConfig;
use crate::http::{RequestHead, header, header::HeaderName};

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

/// `HttpRequest` connection information
#[derive(Debug, Clone, Default)]
pub struct ConnectionInfo {
    scheme: String,
    host: String,
    remote: Option<String>,
    peer: Option<String>,
}

/// Returns `host` if it is a valid `host[:port]`.
fn valid_host(host: &str) -> Option<&str> {
    urly::Authority::new(host)
        .is_ok_and(|a| a.userinfo().is_none() && !a.host().is_empty())
        .then_some(host)
}

impl ConnectionInfo {
    /// Create *`ConnectionInfo`* instance for a request.
    pub fn get<'a>(req: &'a RequestHead, cfg: &'a WebAppConfig) -> Ref<'a, Self> {
        if !req.extensions().contains::<ConnectionInfo>() {
            req.extensions_mut().insert(ConnectionInfo::new(req, cfg));
        }
        Ref::map(req.extensions(), |e| e.get().unwrap())
    }

    fn new(req: &RequestHead, cfg: &WebAppConfig) -> ConnectionInfo {
        let mut host = None;
        let mut scheme = None;
        let mut remote = None;
        let mut peer = None;

        // load forwarded header
        for hdr in req.headers.get_all(&header::FORWARDED) {
            if let Ok(val) = hdr.to_str() {
                for pair in val.split(';') {
                    for el in pair.split(',') {
                        let mut items = el.trim().splitn(2, '=');
                        if let Some(name) = items.next()
                            && let Some(val) = items.next()
                        {
                            match &name.to_lowercase() as &str {
                                "for" if remote.is_none() => {
                                    remote = Some(val.trim());
                                }
                                "proto" if scheme.is_none() => {
                                    scheme = Some(val.trim());
                                }
                                "host" if host.is_none() => {
                                    host = Some(val.trim());
                                }
                                _ => (),
                            }
                        }
                    }
                }
            }
        }

        // scheme
        if scheme.is_none() {
            if let Some(h) = req.headers.get(&X_FORWARDED_PROTO)
                && let Ok(h) = h.to_str()
            {
                scheme = h.split(',').next().map(str::trim);
            }
            if scheme.is_none() {
                scheme = req.uri.scheme_str();
                if scheme.is_none() && cfg.secure() {
                    scheme = Some("https");
                }
            }
        }

        // host, invalid values are skipped
        host = host
            .and_then(valid_host)
            .or_else(|| {
                let h = req.headers.get(&X_FORWARDED_HOST)?.to_str().ok()?;
                valid_host(h.split(',').next()?.trim())
            })
            // the authority of an absolute-form target takes precedence over
            // `Host`, see RFC 9112 section 3.2.2
            .or_else(|| Some(req.uri.authority()?.host_port()).filter(|h| !h.is_empty()))
            .or_else(|| valid_host(req.headers.get(&header::HOST)?.to_str().ok()?))
            .or_else(|| Some(cfg.host()));

        // remote addr
        if remote.is_none() {
            if let Some(h) = req.headers.get(&X_FORWARDED_FOR)
                && let Ok(h) = h.to_str()
            {
                remote = h.split(',').next().map(str::trim);
            }
            if remote.is_none() {
                // get peeraddr from socketaddr
                peer = req.peer_addr().map(|addr| format!("{addr}"));
            }
        }

        ConnectionInfo {
            peer,
            scheme: scheme.unwrap_or("http").to_owned(),
            host: host.unwrap_or("localhost").to_owned(),
            remote: remote.map(ToOwned::to_owned),
        }
    }

    /// Scheme of the request.
    ///
    /// Scheme is resolved through the following headers, in this order:
    ///
    /// - Forwarded
    /// - X-Forwarded-Proto
    /// - Uri
    #[inline]
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Hostname of the request.
    ///
    /// Hostname is resolved through the following headers, in this order:
    ///
    /// - Forwarded
    /// - X-Forwarded-Host
    /// - Uri
    /// - Host
    /// - Server hostname
    ///
    /// Header values that are not a valid `host[:port]` are skipped.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Remote socket addr of client initiated HTTP request.
    ///
    /// The addr is resolved through the following headers, in this order:
    ///
    /// - Forwarded
    /// - X-Forwarded-For
    /// - peer name of opened socket
    ///
    /// # Security
    /// Do not use this function for security purposes, unless you can ensure the Forwarded and
    /// X-Forwarded-For headers cannot be spoofed by the client. If you want the client's socket
    /// address explicitly, use
    /// [`HttpRequest::peer_addr()`](crate::web::HttpRequest::peer_addr) instead.
    #[inline]
    pub fn remote(&self) -> Option<&str> {
        if let Some(ref r) = self.remote {
            Some(r)
        } else if let Some(ref peer) = self.peer {
            Some(peer)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::test::TestRequest;

    #[test]
    fn test_forwarded() {
        let req = TestRequest::default().to_http_request();
        let info = req.connection_info();
        assert_eq!(info.scheme(), "http");
        assert_eq!(info.host(), "localhost:8080");

        let req = TestRequest::default()
            .header(
                header::FORWARDED,
                "for=192.0.2.60; proto=https; by=203.0.113.43; host=rust-lang.org",
            )
            .to_http_request();

        let info = req.connection_info();
        assert_eq!(info.scheme(), "https");
        assert_eq!(info.host(), "rust-lang.org");
        assert_eq!(info.remote(), Some("192.0.2.60"));

        let req = TestRequest::default()
            .header(header::HOST, "rust-lang.org")
            .to_http_request();

        let info = req.connection_info();
        assert_eq!(info.scheme(), "http");
        assert_eq!(info.host(), "rust-lang.org");
        assert_eq!(info.remote(), None);

        let req = TestRequest::default()
            .header(X_FORWARDED_FOR, "192.0.2.60")
            .to_http_request();
        let info = req.connection_info();
        assert_eq!(info.remote(), Some("192.0.2.60"));

        let req = TestRequest::default()
            .header(X_FORWARDED_HOST, "192.0.2.60")
            .to_http_request();
        let info = req.connection_info();
        assert_eq!(info.host(), "192.0.2.60");
        assert_eq!(info.remote(), None);

        let req = TestRequest::default()
            .header(X_FORWARDED_PROTO, "https")
            .to_http_request();
        let info = req.connection_info();
        assert_eq!(info.scheme(), "https");
    }

    #[test]
    fn test_forwarded_ignored_items() {
        let req = TestRequest::default()
            .header(
                header::FORWARDED,
                "for=192.0.2.60, for=192.0.2.61; by=203.0.113.43; host=a.org; host=b.org; proto=https; proto=http; unknown",
            )
            .to_http_request();
        let info = req.connection_info();
        assert_eq!(info.remote(), Some("192.0.2.60"));
        assert_eq!(info.host(), "a.org");
        assert_eq!(info.scheme(), "https");
    }

    #[test]
    fn test_host_sources() {
        // absolute-form target takes precedence over `Host`, userinfo is dropped
        let req = TestRequest::with_uri("http://u:p@a.org:8080/p")
            .header(header::HOST, "b.org")
            .to_http_request();
        assert_eq!(req.connection_info().host(), "a.org:8080");

        // invalid values are skipped
        let req = TestRequest::default()
            .header(header::FORWARDED, "host=a/b")
            .header(X_FORWARDED_HOST, "evil/x#, c.org")
            .header(header::HOST, "b.org:81")
            .to_http_request();
        assert_eq!(req.connection_info().host(), "b.org:81");

        let req = TestRequest::default()
            .header(X_FORWARDED_HOST, "u@c.org")
            .header(header::HOST, "")
            .to_http_request();
        assert_eq!(req.connection_info().host(), "localhost:8080");
    }

    #[test]
    fn test_secure_config() {
        let req = TestRequest::default().to_http_request();
        let info = ConnectionInfo::new(req.head(), &WebAppConfig::new().set_secure());
        assert_eq!(info.scheme(), "https");

        let info = ConnectionInfo::new(req.head(), &WebAppConfig::new());
        assert_eq!(info.scheme(), "http");
    }

    #[crate::rt_test]
    async fn test_peer_addr() {
        let req = TestRequest::default()
            .peer_addr("192.0.2.1:8080".parse().unwrap())
            .to_http_request();
        let info = req.connection_info();
        assert_eq!(info.remote(), Some("192.0.2.1:8080"));

        let req = TestRequest::default()
            .peer_addr("192.0.2.1:8080".parse().unwrap())
            .header(X_FORWARDED_FOR, "192.0.2.60")
            .to_http_request();
        let info = req.connection_info();
        assert_eq!(info.remote(), Some("192.0.2.60"));
    }
}
