//! Route match guards.
//!
//! Guards are one of the ways how ntex web router chooses a
//! handler service. In essence it is just a function that accepts a
//! reference to a `RequestHead` instance and returns a boolean.
//! It is possible to add guards to *scopes*, *resources*
//! and *routes*. ntex provide several guards by default, like various
//! http methods, header, etc. To become a guard, type must implement `Guard`
//! trait. Simple functions can be used as guards as well, see [`fn_guard`].
//!
//! Multiple guards on the same route, resource, or scope must all match.
//!
//! Guards can not modify the request object. But it is possible
//! to store extra attributes on a request by using the `Extensions` container.
//! Extensions containers are available via the `RequestHead::extensions()` method.
//!
//! ```rust
//! use ntex::web::{self, guard, App, HttpResponse};
//!
//! fn main() {
//!     App::default().service(web::resource("/index.html").route(
//!         web::route()
//!              .guard(guard::Post())
//!              .guard(guard::fn_guard(|head| head.headers.contains_key("x-api-key")))
//!              .to(async || { HttpResponse::Ok() }))
//!     );
//! }
//! ```
#![allow(non_snake_case)]

use std::{borrow::Cow, fmt};

use urly::Url;

use crate::http::{Method, RequestHead, header};

/// Trait defines resource guards. Guards are used for route selection.
///
/// Guards can not modify the request object. But it is possible
/// to store extra attributes on a request by using the `Extensions` container.
/// Extensions containers are available via the `RequestHead::extensions()` method.
pub trait Guard {
    /// Check if request matches predicate
    fn check(&self, request: &RequestHead) -> bool;

    /// Debug format
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Guard").finish()
    }
}

/// Create guard object for supplied function.
///
/// ```rust
/// use ntex::web::{self, guard, App, HttpResponse};
///
/// fn main() {
///     App::default().service(web::resource("/index.html").route(
///         web::route()
///             .guard(
///                 guard::fn_guard(
///                     |req| req.headers()
///                              .contains_key("content-type")))
///             .to(async || { HttpResponse::MethodNotAllowed() }))
///     );
/// }
/// ```
pub fn fn_guard<F>(f: F) -> impl Guard
where
    F: Fn(&RequestHead) -> bool,
{
    FnGuard(f)
}

struct FnGuard<F: Fn(&RequestHead) -> bool>(F);

impl<F> Guard for FnGuard<F>
where
    F: Fn(&RequestHead) -> bool,
{
    fn check(&self, head: &RequestHead) -> bool {
        (self.0)(head)
    }

    /// Debug format
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FnGuard")
            .field(&std::any::type_name::<F>())
            .finish()
    }
}

impl<F> fmt::Debug for FnGuard<F>
where
    F: Fn(&RequestHead) -> bool,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Guard::fmt(self, f)
    }
}

impl<F> Guard for F
where
    F: Fn(&RequestHead) -> bool,
{
    fn check(&self, head: &RequestHead) -> bool {
        (self)(head)
    }
}

/// Return guard that matches if any of supplied guards.
///
/// ```rust
/// use ntex::web::{self, guard, App, HttpResponse};
///
/// fn main() {
///     App::default().service(web::resource("/index.html").route(
///         web::route()
///              .guard(guard::Any(guard::Get()).or(guard::Post()))
///              .to(async || { HttpResponse::MethodNotAllowed() }))
///     );
/// }
/// ```
pub fn Any<F: Guard + 'static>(guard: F) -> AnyGuard {
    AnyGuard(vec![Box::new(guard)])
}

#[derive(Default)]
/// Matches any of supplied guards match.
pub struct AnyGuard(pub Vec<Box<dyn Guard>>);

impl AnyGuard {
    #[must_use]
    /// Add guard to a list of guards to check.
    pub fn or<F: Guard + 'static>(mut self, guard: F) -> Self {
        self.0.push(Box::new(guard));
        self
    }
}

impl Guard for AnyGuard {
    fn check(&self, req: &RequestHead) -> bool {
        for p in &self.0 {
            if p.check(req) {
                return true;
            }
        }
        false
    }

    /// Debug format
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AnyGuard(")?;
        self.0.iter().for_each(|t| {
            let _ = t.fmt(f);
        });
        write!(f, ")")
    }
}

impl fmt::Debug for AnyGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Guard::fmt(self, f)
    }
}

/// Return guard that matches if all of the supplied guards.
///
/// ```rust
/// use ntex::web::{self, guard, App, HttpResponse};
///
/// fn main() {
///     App::default().service(web::resource("/index.html").route(
///         web::route()
///             .guard(
///                 guard::All(guard::Get()).and(guard::Header("content-type", "text/plain")))
///             .to(async || { HttpResponse::MethodNotAllowed() }))
///     );
/// }
/// ```
pub fn All<F: Guard + 'static>(guard: F) -> AllGuard {
    AllGuard(vec![Box::new(guard)])
}

#[derive(Default)]
/// Matches all of supplied guards..
pub struct AllGuard(pub(super) Vec<Box<dyn Guard>>);

impl AllGuard {
    #[must_use]
    /// Add new guard to the list of guards to check.
    pub fn and<F: Guard + 'static>(mut self, guard: F) -> Self {
        self.0.push(Box::new(guard));
        self
    }

    /// Add guard to a list of guards to check.
    pub fn add<F: Guard + 'static>(&mut self, guard: F) {
        self.0.push(Box::new(guard));
    }
}

impl Guard for AllGuard {
    fn check(&self, request: &RequestHead) -> bool {
        for p in &self.0 {
            if !p.check(request) {
                return false;
            }
        }
        true
    }

    /// Debug format
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AllGuard(")?;
        self.0.iter().for_each(|t| {
            let _ = t.fmt(f);
        });
        write!(f, ")")
    }
}

impl fmt::Debug for AllGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Guard::fmt(self, f)
    }
}

/// Return guard that matches if supplied guard does not match.
pub fn Not<F: Guard + 'static>(guard: F) -> NotGuard {
    NotGuard(Box::new(guard))
}

#[doc(hidden)]
pub struct NotGuard(Box<dyn Guard>);

impl Guard for NotGuard {
    fn check(&self, request: &RequestHead) -> bool {
        !self.0.check(request)
    }

    /// Debug format
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NotGuard(")?;
        self.0.fmt(f)?;
        write!(f, ")")
    }
}

impl fmt::Debug for NotGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Guard::fmt(self, f)
    }
}

/// Http method guard
#[doc(hidden)]
#[derive(Debug)]
pub struct MethodGuard(Method);

impl Guard for MethodGuard {
    fn check(&self, request: &RequestHead) -> bool {
        request.method == self.0
    }

    /// Debug format
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Guard to match *GET* http method
pub fn Get() -> MethodGuard {
    MethodGuard(Method::GET)
}

/// Predicate to match *POST* http method
pub fn Post() -> MethodGuard {
    MethodGuard(Method::POST)
}

/// Predicate to match *PUT* http method
pub fn Put() -> MethodGuard {
    MethodGuard(Method::PUT)
}

/// Predicate to match *DELETE* http method
pub fn Delete() -> MethodGuard {
    MethodGuard(Method::DELETE)
}

/// Predicate to match *HEAD* http method
pub fn Head() -> MethodGuard {
    MethodGuard(Method::HEAD)
}

/// Predicate to match *OPTIONS* http method
pub fn Options() -> MethodGuard {
    MethodGuard(Method::OPTIONS)
}

/// Predicate to match *CONNECT* http method
pub fn Connect() -> MethodGuard {
    MethodGuard(Method::CONNECT)
}

/// Predicate to match *PATCH* http method
pub fn Patch() -> MethodGuard {
    MethodGuard(Method::PATCH)
}

/// Predicate to match *TRACE* http method
pub fn Trace() -> MethodGuard {
    MethodGuard(Method::TRACE)
}

/// Predicate to match *QUERY* http method
pub fn Query() -> MethodGuard {
    MethodGuard(Method::QUERY)
}

/// Predicate to match specified http method
pub fn Method(method: Method) -> MethodGuard {
    MethodGuard(method)
}

/// Return predicate that matches if request contains specified header and
/// value.
pub fn Header(name: &'static str, value: &'static str) -> HeaderGuard {
    HeaderGuard(
        header::HeaderName::try_from(name).unwrap(),
        header::HeaderValue::from_static(value),
    )
}

#[doc(hidden)]
#[derive(Debug)]
pub struct HeaderGuard(header::HeaderName, header::HeaderValue);

impl Guard for HeaderGuard {
    fn check(&self, req: &RequestHead) -> bool {
        if let Some(val) = req.headers.get(&self.0) {
            return val == self.1;
        }
        false
    }

    /// Debug format
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Return predicate that matches if request contains specified Host name.
///
/// ```rust
/// use ntex::web::{self, guard::Host, App, HttpResponse};
///
/// fn main() {
///     App::default().service(
///         web::resource("/index.html")
///             .guard(Host("www.rust-lang.org"))
///             .to(async || { HttpResponse::MethodNotAllowed() })
///     );
/// }
/// ```
pub fn Host<H: AsRef<str>>(host: H) -> HostGuard {
    HostGuard(host.as_ref().to_string(), None)
}

fn get_host_uri(req: &RequestHead) -> Option<Cow<'_, Url>> {
    // the authority of an absolute-form target takes precedence over `Host`,
    // see RFC 9112 section 3.2.2
    if req.uri.authority().is_some() {
        return Some(Cow::Borrowed(&req.uri));
    }
    let host = req.headers.get(header::HOST)?.to_str().ok()?;
    if host.contains("://") {
        Url::try_from(host).ok().map(Cow::Owned)
    } else {
        Url::parse(host).ok().map(Cow::Owned)
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct HostGuard(String, Option<String>);

impl HostGuard {
    #[must_use]
    /// Set request scheme to match.
    ///
    /// The scheme is taken from the request uri if it is in absolute-form, or
    /// from the `Host` header value otherwise. The guard fails closed: a request
    /// that carries no scheme does not match. Ordinary origin-form requests
    /// (`GET /path` with `Host: example.com`) and authority-form `CONNECT`
    /// requests carry no scheme, so a guard with a scheme rejects them. The
    /// scheme is not derived from the connection, so this does not tell whether
    /// the request arrived over TLS; do not rely on it for security decisions.
    pub fn scheme<H: AsRef<str>>(mut self, scheme: H) -> HostGuard {
        self.1 = Some(scheme.as_ref().to_string());
        self
    }
}

impl Guard for HostGuard {
    fn check(&self, req: &RequestHead) -> bool {
        let Some(req_host_uri) = get_host_uri(req) else {
            return false;
        };

        if let Some(uri_host) = req_host_uri.host() {
            if !self.0.eq_ignore_ascii_case(uri_host) {
                return false;
            }
        } else {
            return false;
        }

        if let Some(ref scheme) = self.1 {
            return req_host_uri
                .scheme_str()
                .is_some_and(|s| scheme.eq_ignore_ascii_case(s));
        }

        true
    }

    /// Debug format
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::test::TestRequest;

    /// STATEMENT: Header guards require an existing header with the expected value.
    ///
    /// HOW:
    /// - Check matching, mismatched, and absent headers against a chunked request.
    /// - Assert only the matching value passes and debug output identifies the header guard.
    #[test]
    fn test_header() {
        let req = TestRequest::with_header(header::TRANSFER_ENCODING, "chunked").to_http_request();

        let pred = Header("transfer-encoding", "chunked");
        assert!(pred.check(req.head()));

        let pred = Header("transfer-encoding", "other");
        assert!(!pred.check(req.head()));

        let pred = Header("content-type", "other");
        assert!(!pred.check(req.head()));

        assert!(format!("{pred:?}").contains("Header"));
    }

    /// STATEMENT: Host guards match a bare host, but a required scheme fails closed when the request carries none.
    ///
    /// HOW:
    /// - Supply `www.rust-lang.org` in `Host` and check guards with and without a scheme.
    /// - Assert the matching host passes without a scheme, `.scheme("https")` rejects it, and unrelated hosts fail.
    #[test]
    fn test_host() {
        let req = TestRequest::default()
            .header(
                header::HOST,
                header::HeaderValue::from_static("www.rust-lang.org"),
            )
            .to_http_request();

        let pred = Host("www.rust-lang.org");
        assert!(pred.check(req.head()));

        // the request carries no scheme, so a scheme check fails closed
        let pred = Host("www.rust-lang.org").scheme("https");
        assert!(!pred.check(req.head()));

        let pred = Host("blog.rust-lang.org");
        assert!(!pred.check(req.head()));

        let pred = Host("blog.rust-lang.org").scheme("https");
        assert!(!pred.check(req.head()));

        let pred = Host("crates.io");
        assert!(!pred.check(req.head()));

        let pred = Host("localhost");
        assert!(!pred.check(req.head()));
    }

    /// STATEMENT: Host guards enforce a supplied request scheme as well as the hostname.
    ///
    /// HOW:
    /// - Supply an HTTPS host value and check matching, omitted, and mismatched guard schemes.
    /// - Assert unrelated hosts fail and debug output identifies the host guard.
    #[test]
    fn test_host_scheme() {
        let req = TestRequest::default()
            .header(
                header::HOST,
                header::HeaderValue::from_static("https://www.rust-lang.org"),
            )
            .to_http_request();

        let pred = Host("www.rust-lang.org").scheme("https");
        assert!(pred.check(req.head()));

        let pred = Host("www.rust-lang.org");
        assert!(pred.check(req.head()));

        let pred = Host("www.rust-lang.org").scheme("http");
        assert!(!pred.check(req.head()));

        let pred = Host("blog.rust-lang.org");
        assert!(!pred.check(req.head()));

        let pred = Host("blog.rust-lang.org").scheme("https");
        assert!(!pred.check(req.head()));

        let pred = Host("crates.io").scheme("https");
        assert!(!pred.check(req.head()));

        let pred = Host("localhost");
        assert!(!pred.check(req.head()));

        assert!(format!("{pred:?}").contains("Host"));
    }

    /// STATEMENT: Host guards use CONNECT authorities, not double-slash origin paths.
    ///
    /// HOW:
    /// - Match an authority-form CONNECT request without a `Host` header.
    /// - Check matching/nonmatching hosts, that a scheme check fails because the target carries no scheme,
    ///   and confirm an origin path does not supply a host.
    #[test]
    fn test_host_without_header() {
        let req = TestRequest::default()
            .method(crate::http::Method::CONNECT)
            .uri("www.rust-lang.org:443")
            .to_http_request();

        let pred = Host("www.rust-lang.org");
        assert!(pred.check(req.head()));

        let pred = Host("www.rust-lang.org").scheme("https");
        assert!(!pred.check(req.head()));

        let pred = Host("blog.rust-lang.org");
        assert!(!pred.check(req.head()));

        let pred = Host("blog.rust-lang.org").scheme("https");
        assert!(!pred.check(req.head()));

        let pred = Host("crates.io");
        assert!(!pred.check(req.head()));

        let pred = Host("localhost");
        assert!(!pred.check(req.head()));

        let req = TestRequest::with_uri("//www.rust-lang.org").to_http_request();
        assert!(!Host("www.rust-lang.org").check(req.head()));
    }

    /// STATEMENT: Absolute request targets override `Host`, while malformed host values do not match.
    ///
    /// HOW:
    /// - Supply conflicting absolute-target and header authorities and assert target host/scheme wins.
    /// - Check a host with a port matches and a host containing a path does not.
    #[test]
    fn test_host_absolute_form() {
        // the target authority takes precedence over `Host`
        let req = TestRequest::with_uri("http://www.rust-lang.org/p")
            .header(header::HOST, "crates.io")
            .to_http_request();
        assert!(Host("www.rust-lang.org").scheme("http").check(req.head()));
        assert!(!Host("www.rust-lang.org").scheme("https").check(req.head()));
        assert!(!Host("crates.io").check(req.head()));

        let req = TestRequest::default()
            .header(header::HOST, "www.rust-lang.org:8080")
            .to_http_request();
        assert!(Host("www.rust-lang.org").check(req.head()));

        let req = TestRequest::default()
            .header(header::HOST, "www.rust-lang.org/p")
            .to_http_request();
        assert!(!Host("www.rust-lang.org").check(req.head()));
    }

    /// STATEMENT: Method guards accept their own HTTP method and reject other methods.
    ///
    /// HOW:
    /// - Build requests for GET, POST, PUT, DELETE, HEAD, OPTIONS, CONNECT, PATCH, TRACE, and QUERY.
    /// - Check each corresponding guard against matching and mismatched requests.
    /// - Assert the trace guard debug output includes its method.
    #[test]
    fn test_methods() {
        let req = TestRequest::default().to_http_request();
        let req2 = TestRequest::default()
            .method(Method::POST)
            .to_http_request();

        assert!(Get().check(req.head()));
        assert!(!Get().check(req2.head()));
        assert!(Post().check(req2.head()));
        assert!(!Post().check(req.head()));

        let r = TestRequest::default().method(Method::PUT).to_http_request();
        assert!(Put().check(r.head()));
        assert!(!Put().check(req.head()));

        let r = TestRequest::default()
            .method(Method::DELETE)
            .to_http_request();
        assert!(Delete().check(r.head()));
        assert!(!Delete().check(req.head()));

        let r = TestRequest::default()
            .method(Method::HEAD)
            .to_http_request();
        assert!(Head().check(r.head()));
        assert!(!Head().check(req.head()));

        let r = TestRequest::default()
            .method(Method::OPTIONS)
            .to_http_request();
        assert!(Options().check(r.head()));
        assert!(!Options().check(req.head()));

        let r = TestRequest::default()
            .method(Method::CONNECT)
            .to_http_request();
        assert!(Connect().check(r.head()));
        assert!(!Connect().check(req.head()));

        let r = TestRequest::default()
            .method(Method::PATCH)
            .to_http_request();
        assert!(Patch().check(r.head()));
        assert!(!Patch().check(req.head()));

        let r = TestRequest::default()
            .method(Method::TRACE)
            .to_http_request();
        assert!(Trace().check(r.head()));
        assert!(!Trace().check(req.head()));
        assert!(format!("{:?}", Trace()).contains("MethodGuard(TRACE)"));

        let r = TestRequest::default()
            .method(Method::QUERY)
            .to_http_request();
        assert!(Query().check(r.head()));
        assert!(!Query().check(req.head()));
    }

    /// STATEMENT: `Not`, `All`, and `Any` compose method guards with their expected Boolean semantics.
    ///
    /// HOW:
    /// - Check negated, conjunctive, and disjunctive guards against a TRACE request.
    /// - Assert positive and negative outcomes and each composite guard's debug identity.
    #[test]
    fn test_preds() {
        let r = TestRequest::default()
            .method(Method::TRACE)
            .to_http_request();

        assert!(Not(Get()).check(r.head()));
        assert!(!Not(Trace()).check(r.head()));
        assert!(format!("{:?}", Not(Get())).contains("NotGuard"));

        assert!(All(Trace()).and(Trace()).check(r.head()));
        assert!(!All(Get()).and(Trace()).check(r.head()));
        assert!(format!("{:?}", All(Get())).contains("AllGuard"));

        assert!(Any(Get()).or(Trace()).check(r.head()));
        assert!(!Any(Get()).or(Get()).check(r.head()));
        assert!(format!("{:?}", Any(Get())).contains("AnyGuard"));
    }

    /// STATEMENT: Function guards and closures can inspect request headers.
    ///
    /// HOW:
    /// - Check a content-type predicate through `fn_guard` and a direct closure.
    /// - Assert both pass and `FnGuard` debug output identifies the wrapper.
    #[test]
    fn test_fn_guard() {
        let req = TestRequest::with_header(header::CONTENT_TYPE, "text/plain").to_http_request();

        let g = fn_guard(|req| req.headers().contains_key("content-type"));
        assert!(g.check(req.head()));
        let g = FnGuard(|req| req.headers().contains_key("content-type"));
        assert!(format!("{g:?}").contains("FnGuard"));

        let g = |req: &RequestHead| req.headers().contains_key("content-type");
        assert!(g.check(req.head()));
    }

    /// STATEMENT: Composite guard debug output includes built-in and custom guard identities.
    ///
    /// HOW:
    /// - Compose a header guard, host guard, and custom `Guard` implementation with `Any`.
    /// - Format it and assert all three guard identities are present.
    #[test]
    fn test_guard_debug() {
        struct Custom;

        impl Guard for Custom {
            fn check(&self, _: &RequestHead) -> bool {
                true
            }
        }

        let guard = Any(Header("content-type", "text/plain"))
            .or(Host("localhost"))
            .or(Custom);
        let s = format!("{guard:?}");
        assert!(s.contains("HeaderGuard"), "{s}");
        assert!(s.contains("HostGuard"), "{s}");
        assert!(s.contains("Guard"), "{s}");
    }

    /// STATEMENT: Invalid or path-only `Host` values cannot satisfy a hostname guard.
    ///
    /// HOW:
    /// - Build requests with a space-containing host and a path-only host.
    /// - Assert neither matches `Host("localhost")`.
    #[test]
    fn test_host_without_host_name() {
        let req = TestRequest::default()
            .header(header::HOST, header::HeaderValue::from_static("bad host"))
            .to_http_request();
        assert!(!Host("localhost").check(req.head()));

        let req = TestRequest::default()
            .header(header::HOST, header::HeaderValue::from_static("/path"))
            .to_http_request();
        assert!(!Host("localhost").check(req.head()));
    }
}
