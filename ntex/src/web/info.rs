use std::{cell::Ref, fmt, net::IpAddr, net::SocketAddr, sync::Arc};

use super::config::WebAppConfig;
use crate::http::{RequestHead, header, header::HeaderName};

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

/// Trust policy for the `Forwarded` and `X-Forwarded-*` request headers.
///
/// Clients can send these headers themselves, so they are only meaningful if
/// they are set by a proxy you control. The policy is configured with
/// [`WebAppConfig::set_forwarded()`] and used by [`ConnectionInfo`].
///
/// The default policy is [`ForwardedPolicy::ignore()`].
///
/// ```rust
/// use ntex::web::{self, App, ForwardedPolicy, HttpRequest, WebAppConfig};
///
/// async fn index(req: HttpRequest) -> String {
///     format!("client: {:?}", req.connection_info().remote())
/// }
///
/// let config = WebAppConfig::new()
///     .set_forwarded(ForwardedPolicy::trust_proxies(["10.0.0.1".parse().unwrap()]));
///
/// App::default()
///     .with_config(config)
///     .route("/", web::get().to(index));
/// ```
#[derive(Clone, Default)]
pub struct ForwardedPolicy(Policy);

#[derive(Clone, Default)]
enum Policy {
    #[default]
    Ignore,
    TrustAll,
    Trust(Arc<dyn Fn(IpAddr) -> bool + Send + Sync>),
}

impl ForwardedPolicy {
    /// Ignore forwarding headers.
    ///
    /// Connection information comes from the connection and the request only:
    /// the peer address, the request target, the `Host` header and the server
    /// configuration. This is the default.
    pub fn ignore() -> Self {
        ForwardedPolicy(Policy::Ignore)
    }

    /// Trust forwarding headers from every peer.
    ///
    /// The first (client-most) value of each header is used, as ntex did before
    /// 4.7.0. Use this only if every connection comes through a proxy that
    /// removes or overwrites client-supplied forwarding headers.
    pub fn trust_all() -> Self {
        ForwardedPolicy(Policy::TrustAll)
    }

    /// Trust forwarding headers only from the listed proxy addresses.
    ///
    /// See [`ForwardedPolicy::trust_fn()`] for how the headers are evaluated.
    pub fn trust_proxies<I>(proxies: I) -> Self
    where
        I: IntoIterator<Item = IpAddr>,
    {
        let proxies: Vec<IpAddr> = proxies.into_iter().map(|ip| ip.to_canonical()).collect();
        Self::trust_fn(move |ip| proxies.contains(&ip))
    }

    /// Trust forwarding headers from proxies for which `f` returns `true`.
    ///
    /// The headers are used only if the peer of the connection is trusted. The
    /// hops listed in `Forwarded`, or in `X-Forwarded-For` if there is no
    /// `Forwarded` header, are then walked from the nearest one, skipping
    /// trusted proxies. The first untrusted hop is the client, or the farthest
    /// hop if all of them are trusted. With `Forwarded` the scheme and host
    /// come from the client's element. With `X-Forwarded-Proto` and
    /// `X-Forwarded-Host` the values are matched to `X-Forwarded-For`
    /// entries counting from the end.
    ///
    /// Trusted proxies must add their own entries, and must not pass along a
    /// client's `X-Forwarded-Proto` or `X-Forwarded-Host` without setting them.
    ///
    /// `f` receives addresses with IPv4-mapped IPv6 addresses converted to IPv4.
    pub fn trust_fn<F>(f: F) -> Self
    where
        F: Fn(IpAddr) -> bool + Send + Sync + 'static,
    {
        ForwardedPolicy(Policy::Trust(Arc::new(f)))
    }
}

impl fmt::Debug for ForwardedPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.0 {
            Policy::Ignore => "Ignore",
            Policy::TrustAll => "TrustAll",
            Policy::Trust(_) => "Trust",
        };
        f.debug_tuple("ForwardedPolicy").field(&name).finish()
    }
}

/// `HttpRequest` connection information
///
/// Values from the `Forwarded` and `X-Forwarded-*` headers are used only as
/// allowed by the application's [`ForwardedPolicy`].
#[derive(Debug, Clone, Default)]
pub struct ConnectionInfo {
    scheme: String,
    host: String,
    remote: Option<String>,
}

/// Returns `host` if it is a valid `host[:port]`.
fn valid_host(host: &str) -> Option<&str> {
    urly::Authority::new(host)
        .is_ok_and(|a| a.userinfo().is_none() && !a.host().is_empty())
        .then_some(host)
}

/// One element of a `Forwarded` header, added by a single proxy.
#[derive(Debug, Default, PartialEq)]
struct Element {
    for_: Option<String>,
    proto: Option<String>,
    host: Option<String>,
}

/// Parse `Forwarded` header values into elements, see RFC 7239 section 4.
///
/// Quoted values are unquoted. Parameters without a value, and repeated
/// parameters within an element, are ignored.
fn parse_forwarded<'a>(values: impl Iterator<Item = &'a str>) -> Vec<Element> {
    let mut elements = Vec::new();
    for val in values {
        let mut el = Element::default();
        let mut rest = val;
        loop {
            let end = rest.find(['=', ';', ',']).unwrap_or(rest.len());
            let name = rest[..end].trim();
            rest = &rest[end..];
            let value = if let Some(after) = rest.strip_prefix('=') {
                let (value, after) = parse_value(after);
                rest = after;
                value
            } else {
                None
            };
            if let Some(value) = value {
                let slot = if name.eq_ignore_ascii_case("for") {
                    Some(&mut el.for_)
                } else if name.eq_ignore_ascii_case("proto") {
                    Some(&mut el.proto)
                } else if name.eq_ignore_ascii_case("host") {
                    Some(&mut el.host)
                } else {
                    None
                };
                if let Some(slot) = slot {
                    slot.get_or_insert(value);
                }
            }
            match rest.as_bytes().first() {
                Some(b';') => rest = &rest[1..],
                Some(b',') => {
                    elements.push(std::mem::take(&mut el));
                    rest = &rest[1..];
                }
                _ => break,
            }
        }
        elements.push(el);
    }
    elements
}

/// Parse a token or a quoted string, returning it and the remaining input
/// starting at the next `;` or `,`.
fn parse_value(input: &str) -> (Option<String>, &str) {
    let input = input.trim_start();
    if let Some(quoted) = input.strip_prefix('"') {
        let mut value = String::new();
        let mut chars = quoted.char_indices();
        while let Some((idx, ch)) = chars.next() {
            match ch {
                '"' => {
                    let rest = &quoted[idx + 1..];
                    let end = rest.find([';', ',']).unwrap_or(rest.len());
                    // nothing but whitespace may follow the closing quote
                    let value = rest[..end].trim().is_empty().then_some(value);
                    return (value, &rest[end..]);
                }
                '\\' => match chars.next() {
                    Some((_, ch)) => value.push(ch),
                    None => break,
                },
                _ => value.push(ch),
            }
        }
        // unterminated quoted string
        (None, "")
    } else {
        let end = input.find([';', ',']).unwrap_or(input.len());
        let value = input[..end].trim();
        ((!value.is_empty()).then(|| value.to_owned()), &input[end..])
    }
}

/// Parse a node address: `ip`, `ip:port`, `[ipv6]` or `[ipv6]:port`.
fn parse_ip(node: &str) -> Option<IpAddr> {
    let ip = if let Ok(addr) = node.parse::<SocketAddr>() {
        addr.ip()
    } else {
        let node = node
            .strip_prefix('[')
            .and_then(|node| node.strip_suffix(']'))
            .unwrap_or(node);
        node.parse().ok()?
    };
    Some(ip.to_canonical())
}

/// Comma separated values of all `name` headers.
fn header_list<'a>(req: &'a RequestHead, name: &HeaderName) -> Vec<&'a str> {
    req.headers
        .get_all(name)
        .filter_map(|val| val.to_str().ok())
        .flat_map(|val| val.split(','))
        .map(str::trim)
        .collect()
}

/// Index of the client in `hops`, walking from the nearest hop and skipping
/// trusted proxies.
fn client_hop<T>(
    hops: &[T],
    node: impl Fn(&T) -> Option<&str>,
    trusted: &dyn Fn(IpAddr) -> bool,
) -> usize {
    (1..hops.len())
        .rev()
        .find(|&idx| !node(&hops[idx]).and_then(parse_ip).is_some_and(trusted))
        .unwrap_or(0)
}

/// The value of a per-hop header list matching the hop that is `from_end`
/// hops before the nearest one, or the farthest value if the list is shorter.
fn aligned<'a>(values: &[&'a str], from_end: usize) -> Option<&'a str> {
    let idx = values.len().checked_sub(1)?.saturating_sub(from_end);
    Some(values[idx]).filter(|val| !val.is_empty())
}

#[derive(Default)]
struct Forwarded {
    scheme: Option<String>,
    host: Option<String>,
    remote: Option<String>,
}

impl Forwarded {
    /// Forwarded values trusted for every peer, the first value of each.
    fn trust_all(req: &RequestHead) -> Self {
        let elements = parse_forwarded(
            req.headers
                .get_all(&header::FORWARDED)
                .filter_map(|val| val.to_str().ok()),
        );
        let first = |f: fn(&Element) -> Option<&String>| elements.iter().find_map(f).cloned();
        let first_xf = |name| header_list(req, &name).first().map(|v| (*v).to_owned());

        let host = first(|el| el.host.as_ref())
            .filter(|h| valid_host(h).is_some())
            .or_else(|| first_xf(X_FORWARDED_HOST).filter(|h| valid_host(h).is_some()));
        Forwarded {
            host,
            scheme: first(|el| el.proto.as_ref()).or_else(|| first_xf(X_FORWARDED_PROTO)),
            remote: first(|el| el.for_.as_ref()).or_else(|| first_xf(X_FORWARDED_FOR)),
        }
    }

    /// Forwarded values set by trusted proxies.
    fn trusted(req: &RequestHead, trusted: &dyn Fn(IpAddr) -> bool) -> Self {
        let peer = req.peer_addr().map(|addr| addr.ip().to_canonical());
        if !peer.is_some_and(trusted) {
            return Forwarded::default();
        }

        if req.headers.contains_key(&header::FORWARDED) {
            let elements = parse_forwarded(
                req.headers
                    .get_all(&header::FORWARDED)
                    .filter_map(|val| val.to_str().ok()),
            );
            let el = &elements[client_hop(&elements, |el| el.for_.as_deref(), trusted)];
            Forwarded {
                scheme: el.proto.clone(),
                host: el.host.clone().filter(|h| valid_host(h).is_some()),
                remote: el.for_.clone(),
            }
        } else {
            let hops = header_list(req, &X_FORWARDED_FOR);
            let (remote, from_end) = if hops.is_empty() {
                (None, 0)
            } else {
                let idx = client_hop(&hops, |hop| Some(*hop), trusted);
                (
                    Some(hops[idx]).filter(|hop| !hop.is_empty()),
                    hops.len() - 1 - idx,
                )
            };
            let aligned = |name| aligned(&header_list(req, &name), from_end).map(ToOwned::to_owned);
            Forwarded {
                scheme: aligned(X_FORWARDED_PROTO),
                host: aligned(X_FORWARDED_HOST).filter(|h| valid_host(h).is_some()),
                remote: remote.map(ToOwned::to_owned),
            }
        }
    }
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
        let fwd = match &cfg.forwarded().0 {
            Policy::Ignore => Forwarded::default(),
            Policy::TrustAll => Forwarded::trust_all(req),
            Policy::Trust(f) => Forwarded::trusted(req, f.as_ref()),
        };

        let scheme = fwd
            .scheme
            .or_else(|| req.uri.scheme_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| if cfg.secure() { "https" } else { "http" }.to_owned());

        let host = fwd.host.unwrap_or_else(|| {
            // the authority of an absolute-form target takes precedence over
            // `Host`, see RFC 9112 section 3.2.2
            req.uri
                .authority()
                .map(urly::Authority::host_port)
                .filter(|h| !h.is_empty())
                .or_else(|| valid_host(req.headers.get(&header::HOST)?.to_str().ok()?))
                .unwrap_or(cfg.host())
                .to_owned()
        });

        let remote = fwd
            .remote
            .or_else(|| req.peer_addr().map(|addr| addr.to_string()));

        ConnectionInfo {
            scheme,
            host,
            remote,
        }
    }

    /// Scheme of the request.
    ///
    /// Scheme is resolved from the following sources, in this order:
    ///
    /// - `Forwarded` or `X-Forwarded-Proto`, if allowed by the [`ForwardedPolicy`]
    /// - Uri
    /// - `https` if the server is configured as secure, `http` otherwise
    #[inline]
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Hostname of the request.
    ///
    /// Hostname is resolved from the following sources, in this order:
    ///
    /// - `Forwarded` or `X-Forwarded-Host`, if allowed by the [`ForwardedPolicy`]
    /// - Uri
    /// - Host
    /// - Server hostname
    ///
    /// Header values that are not a valid `host[:port]` are skipped.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Address of the client that initiated the HTTP request.
    ///
    /// The address is resolved from the following sources, in this order:
    ///
    /// - `Forwarded` or `X-Forwarded-For`, if allowed by the [`ForwardedPolicy`]
    /// - peer address of the connection
    ///
    /// Forwarded values are returned as sent by the proxy, for example
    /// `192.0.2.1`, `[2001:db8::1]:4711` or `unknown`. With the default policy
    /// this is always the peer address. Use
    /// [`HttpRequest::peer_addr()`](crate::web::HttpRequest::peer_addr) for
    /// the address of the connection itself.
    #[inline]
    pub fn remote(&self) -> Option<&str> {
        self.remote.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{HttpRequest, test::TestRequest};

    fn conn_info(req: &HttpRequest, policy: ForwardedPolicy) -> ConnectionInfo {
        ConnectionInfo::new(req.head(), &WebAppConfig::new().set_forwarded(policy))
    }

    fn proxies() -> ForwardedPolicy {
        ForwardedPolicy::trust_proxies(["10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap()])
    }

    /// STATEMENT: By default forwarding headers are ignored and connection information
    /// comes from the connection and the request.
    ///
    /// HOW:
    /// - Send `Forwarded` and every `X-Forwarded-*` header with a `Host` header and a peer
    ///   address through the default configuration.
    /// - Assert the scheme is `http`, the host is the `Host` value and the remote address
    ///   is the peer address.
    #[crate::rt_test]
    async fn test_default_ignores_forwarded() {
        let req = TestRequest::default()
            .peer_addr("192.0.2.1:8080".parse().unwrap())
            .header(header::HOST, "example.com")
            .header(header::FORWARDED, "for=1.2.3.4;proto=https;host=evil.com")
            .header(X_FORWARDED_FOR, "1.2.3.4")
            .header(X_FORWARDED_HOST, "evil.com")
            .header(X_FORWARDED_PROTO, "https")
            .to_http_request();
        let info = req.connection_info();
        assert_eq!(info.scheme(), "http");
        assert_eq!(info.host(), "example.com");
        assert_eq!(info.remote(), Some("192.0.2.1:8080"));
        assert_eq!(
            format!("{:?}", ForwardedPolicy::default()),
            "ForwardedPolicy(\"Ignore\")"
        );
    }

    /// STATEMENT: `ForwardedPolicy::trust_all()` uses defaults and supported forwarding headers.
    ///
    /// HOW:
    /// - Build requests with no headers, `Forwarded`, `Host`, and each `X-Forwarded-*` header.
    /// - Assert the expected scheme, host, and optional remote address for each source.
    #[test]
    fn test_forwarded() {
        let trust = ForwardedPolicy::trust_all;
        let req = TestRequest::default().to_http_request();
        let info = conn_info(&req, trust());
        assert_eq!(info.scheme(), "http");
        assert_eq!(info.host(), "localhost:8080");

        let req = TestRequest::default()
            .header(
                header::FORWARDED,
                "for=192.0.2.60; proto=https; by=203.0.113.43; host=rust-lang.org",
            )
            .to_http_request();
        let info = conn_info(&req, trust());
        assert_eq!(info.scheme(), "https");
        assert_eq!(info.host(), "rust-lang.org");
        assert_eq!(info.remote(), Some("192.0.2.60"));

        let req = TestRequest::default()
            .header(header::HOST, "rust-lang.org")
            .to_http_request();
        let info = conn_info(&req, trust());
        assert_eq!(info.scheme(), "http");
        assert_eq!(info.host(), "rust-lang.org");
        assert_eq!(info.remote(), None);

        let req = TestRequest::default()
            .header(X_FORWARDED_FOR, "192.0.2.60")
            .to_http_request();
        assert_eq!(conn_info(&req, trust()).remote(), Some("192.0.2.60"));

        let req = TestRequest::default()
            .header(X_FORWARDED_HOST, "192.0.2.60")
            .to_http_request();
        let info = conn_info(&req, trust());
        assert_eq!(info.host(), "192.0.2.60");
        assert_eq!(info.remote(), None);

        let req = TestRequest::default()
            .header(X_FORWARDED_PROTO, "https")
            .to_http_request();
        assert_eq!(conn_info(&req, trust()).scheme(), "https");
    }

    /// STATEMENT: With `trust_all()` the first recognized forwarded values are used.
    ///
    /// HOW:
    /// - Supply multiple `for`, `host`, and `proto` values plus ignored items.
    /// - Assert the first remote address, host, and scheme are selected.
    #[test]
    fn test_forwarded_ignored_items() {
        let req = TestRequest::default()
            .header(
                header::FORWARDED,
                "for=192.0.2.60, for=192.0.2.61; by=203.0.113.43; host=a.org; host=b.org; proto=https; proto=http; unknown",
            )
            .to_http_request();
        let info = conn_info(&req, ForwardedPolicy::trust_all());
        assert_eq!(info.remote(), Some("192.0.2.60"));
        assert_eq!(info.host(), "a.org");
        assert_eq!(info.scheme(), "https");
    }

    /// STATEMENT: `Forwarded` values are split into elements and unquoted as in RFC 7239.
    ///
    /// HOW:
    /// - Parse quoted values containing `;`, `,`, escapes and bracketed IPv6 addresses,
    ///   mixed-case names, repeated parameters and multiple header values.
    /// - Assert the parsed elements, and that unterminated quotes, data after a closing
    ///   quote and empty values are dropped.
    #[test]
    fn test_parse_forwarded() {
        let el = |for_: Option<&str>, proto: Option<&str>, host: Option<&str>| Element {
            for_: for_.map(Into::into),
            proto: proto.map(Into::into),
            host: host.map(Into::into),
        };
        assert_eq!(
            parse_forwarded(
                [
                    r#"For="[2001:db8::1]:4711";PROTO="https";host="a;b,c", for=1.2.3.4"#,
                    r#" for = "x\"y" ; for=dup ;proto=http"#,
                ]
                .into_iter()
            ),
            [
                el(Some("[2001:db8::1]:4711"), Some("https"), Some("a;b,c")),
                el(Some("1.2.3.4"), None, None),
                el(Some("x\"y"), Some("http"), None),
            ]
        );
        assert_eq!(
            parse_forwarded([r#"for="1.2.3.4" x;proto="https"#].into_iter()),
            [el(None, None, None)]
        );
        assert_eq!(
            parse_forwarded(["for=;proto=\"\\"].into_iter()),
            [el(None, None, None)]
        );
        assert_eq!(parse_ip("[2001:db8::1]:4711"), "2001:db8::1".parse().ok());
        assert_eq!(parse_ip("[2001:db8::1]"), "2001:db8::1".parse().ok());
        assert_eq!(parse_ip("192.0.2.1:80"), "192.0.2.1".parse().ok());
        assert_eq!(parse_ip("::ffff:192.0.2.1"), "192.0.2.1".parse().ok());
        assert_eq!(parse_ip("unknown"), None);
    }

    /// STATEMENT: Quoted `Forwarded` values are returned without quotes.
    ///
    /// HOW:
    /// - Send quoted `for`, `proto` and `host` values with `trust_all()`.
    /// - Assert `remote()`, `scheme()` and `host()` return the unquoted values.
    #[test]
    fn test_forwarded_quoted() {
        let req = TestRequest::default()
            .header(
                header::FORWARDED,
                r#"for="[2001:db8::1]:4711";proto="https";host="example.com:8443""#,
            )
            .to_http_request();
        let info = conn_info(&req, ForwardedPolicy::trust_all());
        assert_eq!(info.remote(), Some("[2001:db8::1]:4711"));
        assert_eq!(info.scheme(), "https");
        assert_eq!(info.host(), "example.com:8443");
    }

    /// STATEMENT: Trusted proxies' `X-Forwarded-*` headers are used only from a trusted
    /// peer, and the client is the nearest untrusted hop.
    ///
    /// HOW:
    /// - Send several spoofed leading entries through trusted peers, including an IPv4-mapped peer,
    ///   and assert the nearest untrusted `X-Forwarded-For` hop with the matching
    ///   `X-Forwarded-Proto` value and a shorter `X-Forwarded-Host` list clamped to its
    ///   first value.
    /// - Assert the farthest hop is used when every hop is trusted, the last values are used
    ///   without `X-Forwarded-For`, and untrusted or missing peers ignore the headers.
    #[crate::rt_test]
    async fn test_trust_proxies_x_forwarded() {
        let req = |peer: &str| {
            TestRequest::default()
                .peer_addr(peer.parse().unwrap())
                .header(header::HOST, "internal")
                .header(X_FORWARDED_FOR, "6.6.6.6, 7.7.7.7, 203.0.113.9")
                .header(X_FORWARDED_FOR, "10.0.0.2")
                .header(X_FORWARDED_PROTO, "spoof, https, http")
                .header(X_FORWARDED_HOST, "example.com")
                .to_http_request()
        };
        for peer in ["10.0.0.1:1000", "[::ffff:10.0.0.1]:1000"] {
            let info = conn_info(&req(peer), proxies());
            assert_eq!(info.remote(), Some("203.0.113.9"), "{peer}");
            assert_eq!(info.scheme(), "https");
            assert_eq!(info.host(), "example.com");
        }

        let info = conn_info(&req("192.0.2.1:1000"), proxies());
        assert_eq!(info.remote(), Some("192.0.2.1:1000"));
        assert_eq!(info.scheme(), "http");
        assert_eq!(info.host(), "internal");

        let req = TestRequest::default()
            .header(X_FORWARDED_FOR, "203.0.113.9")
            .to_http_request();
        assert_eq!(conn_info(&req, proxies()).remote(), None);

        let req = TestRequest::default()
            .peer_addr("10.0.0.1:1000".parse().unwrap())
            .header(X_FORWARDED_FOR, "10.0.0.2, 10.0.0.1")
            .to_http_request();
        assert_eq!(conn_info(&req, proxies()).remote(), Some("10.0.0.2"));

        let req = TestRequest::default()
            .peer_addr("10.0.0.1:1000".parse().unwrap())
            .header(X_FORWARDED_PROTO, "http, https")
            .header(X_FORWARDED_HOST, "a.org, b.org")
            .to_http_request();
        let info = conn_info(&req, proxies());
        assert_eq!(info.remote(), Some("10.0.0.1:1000"));
        assert_eq!(info.scheme(), "https");
        assert_eq!(info.host(), "b.org");
    }

    /// STATEMENT: Empty or invalid values from trusted proxies fall back to the request,
    /// and listed IPv4-mapped proxy addresses match IPv4 peers.
    ///
    /// HOW:
    /// - Trust only `::ffff:10.0.0.1` and send an empty `X-Forwarded-Proto` entry and an
    ///   invalid `X-Forwarded-Host` from peer `10.0.0.1`.
    /// - Assert the forwarded client is used while the scheme and host come from the
    ///   request, then repeat with an invalid `Forwarded` host.
    #[crate::rt_test]
    async fn test_trust_proxies_invalid_values() {
        let mapped = || ForwardedPolicy::trust_proxies(["::ffff:10.0.0.1".parse().unwrap()]);
        let req = TestRequest::default()
            .peer_addr("10.0.0.1:1000".parse().unwrap())
            .header(header::HOST, "internal")
            .header(X_FORWARDED_FOR, "203.0.113.9")
            .header(X_FORWARDED_PROTO, "https, ")
            .header(X_FORWARDED_HOST, "evil/x")
            .to_http_request();
        let info = conn_info(&req, mapped());
        assert_eq!(info.remote(), Some("203.0.113.9"));
        assert_eq!(info.scheme(), "http");
        assert_eq!(info.host(), "internal");

        let req = TestRequest::default()
            .peer_addr("10.0.0.1:1000".parse().unwrap())
            .header(header::HOST, "internal")
            .header(header::FORWARDED, "for=203.0.113.9;host=evil/x")
            .to_http_request();
        let info = conn_info(&req, mapped());
        assert_eq!(info.remote(), Some("203.0.113.9"));
        assert_eq!(info.host(), "internal");
    }

    /// STATEMENT: With trusted proxies, `Forwarded` takes precedence over `X-Forwarded-*`
    /// and the client's element supplies the scheme and host.
    ///
    /// HOW:
    /// - Send a spoofed first element, a client element with a quoted IPv6 node and a
    ///   trusted proxy element, plus a conflicting `X-Forwarded-For`.
    /// - Assert the remote address, scheme and host of the client element, that an
    ///   unparsable node stops the walk, and that `trust_fn()` predicates are applied.
    #[crate::rt_test]
    async fn test_trust_proxies_forwarded() {
        let req = TestRequest::default()
            .peer_addr("10.0.0.1:1000".parse().unwrap())
            .header(
                header::FORWARDED,
                r#"for=6.6.6.6;proto=http;host=evil.com, for="[2001:db8::1]:4711";proto=https;host=example.com"#,
            )
            .header(header::FORWARDED, "for=10.0.0.2;proto=http;host=internal")
            .header(X_FORWARDED_FOR, "7.7.7.7")
            .to_http_request();
        let info = conn_info(&req, proxies());
        assert_eq!(info.remote(), Some("[2001:db8::1]:4711"));
        assert_eq!(info.scheme(), "https");
        assert_eq!(info.host(), "example.com");

        let req = TestRequest::default()
            .peer_addr("10.0.0.1:1000".parse().unwrap())
            .header(header::FORWARDED, "for=6.6.6.6, for=_hidden, for=10.0.0.2")
            .to_http_request();
        assert_eq!(conn_info(&req, proxies()).remote(), Some("_hidden"));

        let req = TestRequest::default()
            .peer_addr("127.0.0.1:1000".parse().unwrap())
            .header(header::FORWARDED, "for=6.6.6.6, for=127.0.0.2")
            .to_http_request();
        let loopback = || ForwardedPolicy::trust_fn(|ip| ip.is_loopback());
        assert_eq!(conn_info(&req, loopback()).remote(), Some("6.6.6.6"));
        assert_eq!(conn_info(&req, proxies()).remote(), Some("127.0.0.1:1000"));
    }

    /// STATEMENT: Host selection respects absolute targets and skips invalid authority values.
    ///
    /// HOW:
    /// - Combine an absolute target containing userinfo with a conflicting `Host` header.
    /// - Assert the target authority wins without userinfo, then check invalid forwarded hosts
    ///   fall back to `Host` or the configured default with `trust_all()`.
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
        assert_eq!(
            conn_info(&req, ForwardedPolicy::trust_all()).host(),
            "b.org:81"
        );

        let req = TestRequest::default()
            .header(X_FORWARDED_HOST, "u@c.org")
            .header(header::HOST, "")
            .to_http_request();
        assert_eq!(
            conn_info(&req, ForwardedPolicy::trust_all()).host(),
            "localhost:8080"
        );
    }

    /// STATEMENT: Secure application configuration determines the default connection scheme.
    ///
    /// HOW:
    /// - Create `ConnectionInfo` for the same request with secure and ordinary configurations.
    /// - Assert their schemes are `https` and `http`, respectively.
    #[test]
    fn test_secure_config() {
        let req = TestRequest::default().to_http_request();
        let info = ConnectionInfo::new(req.head(), &WebAppConfig::new().set_secure());
        assert_eq!(info.scheme(), "https");

        let info = ConnectionInfo::new(req.head(), &WebAppConfig::new());
        assert_eq!(info.scheme(), "http");
    }

    /// STATEMENT: A forwarded remote address replaces the peer address only if trusted.
    ///
    /// HOW:
    /// - Build requests with a peer address, with and without `X-Forwarded-For`.
    /// - Assert the remote value is the peer socket by default and the forwarded address
    ///   with `trust_all()`.
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
        assert_eq!(req.connection_info().remote(), Some("192.0.2.1:8080"));
        assert_eq!(
            conn_info(&req, ForwardedPolicy::trust_all()).remote(),
            Some("192.0.2.60")
        );
    }
}
