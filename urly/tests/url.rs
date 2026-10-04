use std::net::{Ipv4Addr, Ipv6Addr};

use ntex_bytes::ByteString;
use urly::quoting::{Component, quote, unquote};
use urly::{ErrorKind, Host, Parts, Url};

fn url(s: &str) -> Url {
    Url::parse(s).unwrap_or_else(|e| panic!("{s:?}: {e}"))
}

#[test]
fn normalization() {
    let cases = [
        ("HTTP://EXAMPLE.COM", "http://example.com/"),
        ("  http://example.com/a\tb\n  ", "http://example.com/ab"),
        ("http://example.com:8080", "http://example.com:8080/"),
        ("http://example.com:/a", "http://example.com/a"),
        ("http://example.com/a/./b/../../c", "http://example.com/c"),
        ("http://example.com/../a", "http://example.com/a"),
        (
            "http://example.com/%7euser/%2f",
            "http://example.com/~user/%2F",
        ),
        (
            "http://example.com/a b?c d#e f",
            "http://example.com/a%20b?c+d#e%20f",
        ),
        ("http://example.com/ä", "http://example.com/%C3%A4"),
        ("http://example.com/%zz", "http://example.com/%25zz"),
        ("http://user:p%61ss@Host/", "http://user:pass@host/"),
        ("http://[::FFFF:1.2.3.4]/", "http://[::ffff:1.2.3.4]/"),
        ("http://München.DE/", "http://xn--mnchen-3ya.de/"),
        ("http://xn--MNCHEN-3ya.de/", "http://xn--mnchen-3ya.de/"),
        ("http://%65xample.com/", "http://example.com/"),
        ("custom:opaque", "custom:opaque"),
        ("custom://host", "custom://host"),
        ("file:///etc/hosts", "file:///etc/hosts"),
        ("mailto:User@Example.com", "mailto:User@Example.com"),
        ("a/./b/../c", "a/./b/../c"),
        ("/a/./b/../c", "/a/c"),
        ("?q", "?q"),
        ("#f", "#f"),
        ("//host/p", "//host/p"),
    ];
    for (src, expected) in cases {
        let u = url(src);
        assert_eq!(u, expected, "{src:?}");
        Url::validate(u.as_str()).unwrap_or_else(|e| panic!("{src:?} -> {u}: {e}"));
        assert_eq!(url(u.as_str()), u, "{src:?} is not idempotent");
    }
}

#[test]
fn invalid() {
    for (src, kind) in [
        ("", ErrorKind::Empty),
        ("  ", ErrorKind::Empty),
        ("http://exa mple.com/", ErrorKind::InvalidChar(' ')),
        ("http://[::1]x/", ErrorKind::InvalidIpv6),
        ("http://[::1/", ErrorKind::InvalidIpv6),
        ("http://[zz]/", ErrorKind::InvalidIpv6),
        ("http://h:99999/", ErrorKind::PortOutOfRange),
        ("http://h:8a/", ErrorKind::InvalidPort),
    ] {
        assert_eq!(Url::parse(src).unwrap_err().kind(), kind, "{src:?}");
    }
}

#[test]
fn validate() {
    assert!(Url::validate("http://example.com/a?b#c").is_ok());
    assert!(Url::validate("/relative").is_ok());
    assert!(Url::validate(b"http://h/a?b#c").is_ok());
    assert!(Url::validate(ntex_bytes::Bytes::from_static(b"/a")).is_ok());
    for (src, c, pos) in [
        (&b"/a\xff"[..], char::REPLACEMENT_CHARACTER, 2),
        (&b"/\xc3\xbc\xff"[..], '\u{fc}', 1),
        (&b"http://h/\xc3"[..], char::REPLACEMENT_CHARACTER, 9),
    ] {
        let err = Url::validate(src).unwrap_err();
        assert_eq!(
            (err.kind(), err.position()),
            (ErrorKind::InvalidChar(c), Some(pos))
        );
    }

    for (src, kind, pos) in [
        ("http://ex ample.com", ErrorKind::InvalidChar(' '), Some(9)),
        (
            "http://example.com/a b",
            ErrorKind::InvalidChar(' '),
            Some(20),
        ),
        (
            "http://example.com/%zz",
            ErrorKind::InvalidPercentEncoding,
            Some(19),
        ),
        (
            "http://example.com/?a#b#c",
            ErrorKind::InvalidChar('#'),
            Some(23),
        ),
        (
            "http://example.com/ä",
            ErrorKind::InvalidChar('ä'),
            Some(19),
        ),
        ("http://h:99999/", ErrorKind::PortOutOfRange, None),
        ("1http://h/", ErrorKind::InvalidScheme, None),
        ("", ErrorKind::Empty, None),
    ] {
        let err = Url::validate(src).unwrap_err();
        assert_eq!(err.kind(), kind, "{src:?}");
        if pos.is_some() {
            assert_eq!(err.position(), pos, "{src:?}");
        }
    }
}

#[test]
fn zero_copy() {
    let src = "http://example.com/a?b#c";
    let u = Url::from_static(src);
    assert_eq!(u.as_str().as_ptr(), src.as_ptr());

    let data = ByteString::from_static("https://example.com/path");
    let u = Url::try_from(&data).unwrap();
    assert_eq!(u.as_str().as_ptr(), data.as_ptr());

    // unchanged parts keep sharing the buffer
    let mut u2 = u.clone();
    u2.set_path("/path");
    assert_eq!(u2.as_str().as_ptr(), data.as_ptr());
}

#[test]
fn accessors() {
    let u = url("https://user:pa%20ss@example.com:8443/a/b.tar.gz?x=1&y=2&x=3#frag");
    assert_eq!(u.scheme().unwrap(), "https");
    assert_eq!(u.authority().unwrap(), "user:pa%20ss@example.com:8443");
    assert_eq!(u.userinfo().unwrap(), "user:pa%20ss");
    assert_eq!(u.username().unwrap(), "user");
    assert_eq!(u.password().unwrap(), "pa ss");
    assert_eq!(u.host(), Some("example.com"));
    assert_eq!(u.host_parsed(), Some(Host::Domain("example.com")));
    assert_eq!(u.port().unwrap(), 8443);
    assert_eq!(u.port().unwrap().as_str(), "8443");
    assert_eq!(u.port_or_known_default(), Some(8443));
    assert!(!u.is_default_port());
    assert_eq!(u.path(), "/a/b.tar.gz");
    assert_eq!(u.path().file_name(), Some("b.tar.gz"));
    assert_eq!(u.path().file_stem(), Some("b.tar"));
    assert_eq!(u.path().extension(), Some("gz"));
    assert_eq!(u.path().extensions(), Some("tar.gz"));
    assert_eq!(u.path().segments().collect::<Vec<_>>(), ["a", "b.tar.gz"]);
    assert_eq!(
        u.path().segments().rev().collect::<Vec<_>>(),
        ["b.tar.gz", "a"]
    );
    assert_eq!(u.path_and_query(), "/a/b.tar.gz?x=1&y=2&x=3");
    let q = u.query().unwrap();
    assert_eq!(q, "x=1&y=2&x=3");
    assert_eq!(q.get("x").unwrap(), "1");
    assert_eq!(q.get_all("x").collect::<Vec<_>>(), ["1", "3"]);
    assert!(q.contains_key("y"));
    assert!(!q.contains_key("z"));
    assert_eq!(u.fragment().unwrap(), "frag");
    assert!(u.is_absolute());

    let u = url("http://127.0.0.1/");
    assert_eq!(u.host_parsed(), Some(Host::Ipv4(Ipv4Addr::LOCALHOST)));
    assert_eq!(u.port_or_known_default(), Some(80));
    assert!(u.is_default_port());
    let u = url("http://[::1]:80/");
    assert_eq!(u.host(), Some("[::1]"));
    assert_eq!(u.host_parsed(), Some(Host::Ipv6(Ipv6Addr::LOCALHOST)));
    assert!(u.is_default_port());

    for (src, port) in [
        ("amqp://host", 5672),
        ("amqps://host", 5671),
        ("mqtt://host", 1883),
        ("mqtts://host", 8883),
    ] {
        let u = url(src);
        // not special: an empty path is kept (AMQP vhost semantics)
        assert_eq!(u, src);
        assert_eq!(u.port_or_known_default(), Some(port), "{src}");
    }
    assert!(url("mqtt://host:1883").is_default_port());

    let u = url("http://xn--mnchen-3ya.de/");
    assert_eq!(u.host_parsed().unwrap().to_unicode(), "münchen.de");

    let u = url("/a?");
    assert!(!u.is_absolute());
    assert_eq!(u.scheme(), None);
    assert_eq!(u.host(), None);
    assert_eq!(u.query().unwrap(), "");
    assert_eq!(u.fragment(), None);
}

#[test]
fn derived() {
    let u = url("https://user@example.com:8443/a/b/c?q#f");
    assert_eq!(u.origin().unwrap(), "https://example.com:8443/");
    assert_eq!(u.relative(), "/a/b/c?q#f");
    assert_eq!(u.parent(), "https://user@example.com:8443/a/b");
    assert_eq!(
        u.parent().parent().parent(),
        "https://user@example.com:8443/"
    );
    assert_eq!(url("http://h/a/b/").parent(), "http://h/a");
    assert!(url("/a").origin().is_none());

    assert_eq!(&u / "d e", "https://user@example.com:8443/a/b/c/d%20e");
    assert_eq!(&u / "x/y", "https://user@example.com:8443/a/b/c/x/y");
    assert_eq!(&u / "../d", "https://user@example.com:8443/a/b/d");
    assert_eq!(url("http://h") / "a" / "b", "http://h/a/b");
    assert_eq!(
        url("http://h/a/").push_segments(["b", "c"]),
        "http://h/a/b/c"
    );
}

#[test]
fn join_rfc3986() {
    let base = url("http://a/b/c/d;p?q");
    let cases = [
        ("g:h", "g:h"),
        ("g", "http://a/b/c/g"),
        ("./g", "http://a/b/c/g"),
        ("g/", "http://a/b/c/g/"),
        ("/g", "http://a/g"),
        ("//g", "http://g/"),
        ("?y", "http://a/b/c/d;p?y"),
        ("g?y", "http://a/b/c/g?y"),
        ("#s", "http://a/b/c/d;p?q#s"),
        ("g#s", "http://a/b/c/g#s"),
        ("g?y#s", "http://a/b/c/g?y#s"),
        (";x", "http://a/b/c/;x"),
        ("g;x", "http://a/b/c/g;x"),
        ("g;x?y#s", "http://a/b/c/g;x?y#s"),
        ("", "http://a/b/c/d;p?q"),
        (".", "http://a/b/c/"),
        ("./", "http://a/b/c/"),
        ("..", "http://a/b/"),
        ("../", "http://a/b/"),
        ("../g", "http://a/b/g"),
        ("../..", "http://a/"),
        ("../../", "http://a/"),
        ("../../g", "http://a/g"),
        // abnormal
        ("../../../g", "http://a/g"),
        ("../../../../g", "http://a/g"),
        ("/./g", "http://a/g"),
        ("/../g", "http://a/g"),
        ("g.", "http://a/b/c/g."),
        (".g", "http://a/b/c/.g"),
        ("g..", "http://a/b/c/g.."),
        ("..g", "http://a/b/c/..g"),
        ("./../g", "http://a/b/g"),
        ("./g/.", "http://a/b/c/g/"),
        ("g/./h", "http://a/b/c/g/h"),
        ("g/../h", "http://a/b/c/h"),
        ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
        ("g;x=1/../y", "http://a/b/c/y"),
        ("g?y/./x", "http://a/b/c/g?y/./x"),
        ("g?y/../x", "http://a/b/c/g?y/../x"),
        ("g#s/./x", "http://a/b/c/g#s/./x"),
        ("g#s/../x", "http://a/b/c/g#s/../x"),
        ("http:g", "http:g"),
    ];
    for (r, expected) in cases {
        let joined = base.join(r).unwrap_or_else(|e| panic!("{r:?}: {e}"));
        assert_eq!(joined, expected, "{r:?}");
        if !r.is_empty() {
            assert_eq!(base.join_url(&url(r)), expected, "{r:?}");
        }
    }
    assert_eq!(url("http://a/b#f").join("").unwrap(), "http://a/b");
}

#[test]
fn setters() {
    let mut u = url("http://example.com/a/b.txt?q=1#f");
    u.set_scheme("HTTPS").unwrap();
    assert_eq!(u, "https://example.com/a/b.txt?q=1#f");
    assert!(u.set_scheme("1x").is_err());

    u.set_port(Some(8443)).unwrap();
    assert_eq!(u, "https://example.com:8443/a/b.txt?q=1#f");
    u.set_userinfo(Some("u"), None).unwrap();
    assert_eq!(u, "https://u@example.com:8443/a/b.txt?q=1#f");
    u.set_host("Example.ORG").unwrap();
    assert_eq!(u, "https://u@example.org:8443/a/b.txt?q=1#f");
    u.set_userinfo(None, None).unwrap();
    u.set_port(None).unwrap();
    assert_eq!(u, "https://example.org/a/b.txt?q=1#f");
    assert!(u.set_host("a b").is_err());

    u.set_file_name("c d.json").unwrap();
    assert_eq!(u, "https://example.org/a/c%20d.json?q=1#f");
    u.set_extension("tar.gz").unwrap();
    assert_eq!(u, "https://example.org/a/c%20d.tar.gz?q=1#f");
    u.set_extension("").unwrap();
    assert_eq!(u, "https://example.org/a/c%20d.tar?q=1#f");
    assert!(u.set_file_name("x/y").is_err());

    u.set_path("x/../y z");
    assert_eq!(u, "https://example.org/y%20z?q=1#f");
    u.set_query(None);
    u.set_fragment(None);
    assert_eq!(u, "https://example.org/y%20z");
    u.set_query(Some("a b&c"));
    u.set_fragment(Some("x y"));
    assert_eq!(u, "https://example.org/y%20z?a+b&c#x%20y");

    u.set_authority(None).unwrap();
    assert_eq!(u, "https:/y%20z?a+b&c#x%20y");
    u.set_authority(Some("H:81")).unwrap();
    assert_eq!(u, "https://h:81/y%20z?a+b&c#x%20y");

    let mut u = url("/rel");
    assert_eq!(
        u.set_port(Some(1)).unwrap_err().kind(),
        ErrorKind::AuthorityMissing
    );
    u.set_host("h").unwrap();
    assert_eq!(u, "//h/rel");

    // path that looks like an authority or a scheme
    let mut u = url("a:b");
    u.set_path("//x");
    assert_eq!(u, "a:/.//x");
    assert_eq!(u.path(), "/.//x");
    let mut u = url("/a");
    u.set_path("c:d");
    assert_eq!(u, "./c:d");
}

#[test]
fn query_pairs() {
    let mut u = url("http://h/p");
    u.set_query_pairs([("a", "1 2"), ("b&", "="), ("c", "ü")]);
    assert_eq!(u, "http://h/p?a=1+2&b%26=%3D&c=%C3%BC");
    let q = u.query().unwrap();
    let pairs: Vec<(ByteString, ByteString)> = q.pairs().collect();
    assert_eq!(pairs[0].1, "1 2");
    assert_eq!(pairs[1].0, "b&");
    assert_eq!(pairs[1].1, "=");
    assert_eq!(pairs[2].1, "ü");

    u.extend_query_pairs([("a", "3")]);
    assert_eq!(u, "http://h/p?a=1+2&b%26=%3D&c=%C3%BC&a=3");
    u.update_query_pairs([("a", "x")]);
    assert_eq!(u, "http://h/p?b%26=%3D&c=%C3%BC&a=x");
    u.remove_query_params(["b&", "c"]);
    assert_eq!(u, "http://h/p?a=x");
    u.set_query_pairs(Vec::<(&str, &str)>::new());
    assert_eq!(u, "http://h/p");
}

#[test]
fn quoting() {
    assert_eq!(quote("a b/ü?", Component::Path), "a%20b/%C3%BC%3F");
    assert_eq!(quote("a b&=", Component::QueryPart), "a+b%26%3D");
    assert_eq!(quote("a+b c&d", Component::Query), "a%2Bb+c&d");
    assert_eq!(quote("u@s:r", Component::UserInfo), "u%40s%3Ar");
    assert_eq!(unquote("a+b%26", Component::QueryPart), "a b&");
    assert_eq!(unquote("a+b%2F", Component::Path), "a+b/");
    assert_eq!(unquote("%zz%41", Component::Fragment), "%zzA");
    for s in ["", "abc", "a b", "%", "%%41", "ü/ä?#&=+"] {
        for c in [
            Component::Path,
            Component::QueryPart,
            Component::Fragment,
            Component::UserInfo,
        ] {
            if c == Component::Path && s.contains("%2F") {
                continue;
            }
            assert_eq!(unquote(&quote(s, c), c), s, "{s:?} {c:?}");
        }
    }
}

#[test]
fn builder_and_parts() {
    let u = Url::builder()
        .scheme("http")
        .host("::1")
        .port(8080)
        .path("a")
        .query("x=1")
        .query_pairs([("y", "2")])
        .build()
        .unwrap();
    assert_eq!(u, "http://[::1]:8080/a?x=1&y=2");
    assert_eq!(Url::builder().build().unwrap_err().kind(), ErrorKind::Empty);
    assert!(Url::builder().port(1).build().is_err());

    for src in [
        "http://u@h:1/a?b#c",
        "http://h/",
        "mailto:a@b",
        "/a?",
        "a",
        "#",
        "//h",
        "a:/.//x",
        "./c:d",
    ] {
        let u = url(src);
        let parts = u.clone().into_parts();
        assert_eq!(
            Url::from_parts(parts.clone()).unwrap(),
            u,
            "{src:?} {parts:?}"
        );
    }

    let parts =
        |scheme: Option<&'static str>, authority: Option<&'static str>, pq: &'static str| {
            Url::from_parts(Parts {
                scheme: scheme.map(ByteString::from_static),
                authority: authority.map(ByteString::from_static),
                path_and_query: ByteString::from_static(pq),
                fragment: None,
            })
        };
    assert!(parts(Some("http"), Some("h"), "/a").is_ok());
    assert!(parts(Some("http"), Some("h"), "a").is_err());
    assert!(parts(Some("http"), None, "//a").is_err());
    assert!(parts(None, None, "a:b").is_err());
    assert!(parts(Some("1"), None, "/").is_err());
    assert!(parts(None, Some("h h"), "/").is_err());
    assert!(parts(None, None, "/a b").is_err());
    assert_eq!(parts(None, None, "").unwrap_err().kind(), ErrorKind::Empty);
}

#[test]
fn conversions() {
    let u: Url = "http://h/a".parse().unwrap();
    assert_eq!(Url::try_from("http://h/a").unwrap(), u);
    assert_eq!(Url::try_from(String::from("http://h/a")).unwrap(), u);
    assert_eq!(Url::try_from(&b"http://h/a"[..]).unwrap(), u);
    assert!(Url::try_from(&b"http://h/\xff"[..]).is_err());
    assert_eq!(u.to_string(), "http://h/a");
    assert_eq!(format!("{u:?}"), "\"http://h/a\"");
    assert_eq!(String::from(u.clone()), "http://h/a");
    assert_eq!(ByteString::from(u.clone()), "http://h/a");
    assert!(url("http://h/a") < url("http://h/b"));

    let mut set = std::collections::HashSet::new();
    set.insert(url("HTTP://H/a"));
    assert!(set.contains(&url("http://h/a")));
}

#[test]
fn long_urls() {
    let max = format!("http://h/{}", "a".repeat(65_535 - 9));
    let u = url(&max);
    assert_eq!(u.as_str().len(), 65_535);
    assert_eq!(Url::validate(max.as_bytes()), Ok(()));

    let err = Url::try_from(format!("{max}a")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TooLong);
    assert_eq!(
        Url::validate(format!("{max}a").as_bytes())
            .unwrap_err()
            .kind(),
        ErrorKind::TooLong
    );
    // normalization can grow a URL past the limit
    let err = Url::try_from(format!("{}a b", &max[..max.len() - 3])).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TooLong);

    let mut u = u;
    assert_eq!(
        u.set_extension("txt").unwrap_err().kind(),
        ErrorKind::TooLong
    );
    assert_eq!(u.as_str().len(), 65_535);
    assert!(std::panic::catch_unwind(|| &url(&max) / "a").is_err());
}

#[cfg(feature = "serde")]
#[test]
fn serde() {
    let u = url("http://h/a b");
    let json = serde_json::to_string(&u).unwrap();
    assert_eq!(json, "\"http://h/a%20b\"");
    let u2: Url = serde_json::from_str(&json).unwrap();
    assert_eq!(u, u2);
    let u3: Url = serde_json::from_str("\"HTTP://H/x\"").unwrap();
    assert_eq!(u3, "http://h/x");
    assert!(serde_json::from_str::<Url>("\"\"").is_err());
}

#[cfg(feature = "http")]
#[test]
fn http() {
    use http::Uri;

    let u = url("http://user@h:8080/a?b#c");
    let uri = Uri::try_from(&u).unwrap();
    assert_eq!(uri, "http://user@h:8080/a?b");
    let back = Url::try_from(uri).unwrap();
    assert_eq!(back, "http://user@h:8080/a?b");

    let uri = Uri::try_from(url("/a?b")).unwrap();
    assert_eq!(uri, "/a?b");
    assert_eq!(Url::try_from(Uri::from_static("*")).unwrap(), "*");
}

#[test]
fn parse_invariants() {
    const PIECES: &[&str] = &[
        "http:",
        "HTTPS:",
        "x:",
        "//",
        "/",
        "/",
        "a",
        "B",
        "1",
        ".",
        "..",
        "./",
        "../",
        ":",
        "@",
        "?",
        "#",
        "&",
        "=",
        "+",
        ";",
        "%",
        "%2",
        "%2F",
        "%41",
        "%zz",
        " ",
        "\t",
        "ü",
        "例",
        "[",
        "]",
        "::1",
        "[::1]",
        "127.0.0.1",
        ":80",
        ":0",
        "~",
        "!",
        "'",
        "\\",
        "\"",
        "xn--",
        "%00",
        "%C3%BC",
    ];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..50_000 {
        let n = (next() % 12) as usize;
        let src: String = (0..n)
            .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
            .collect();
        let Ok(u) = Url::parse(&src) else { continue };
        Url::validate(u.as_str()).unwrap_or_else(|e| panic!("{src:?} -> {u:?}: {e}"));
        let again = Url::parse(u.as_str()).unwrap_or_else(|e| panic!("{src:?} -> {u:?}: {e}"));
        assert_eq!(again, u, "{src:?}");
        let parts = u.clone().into_parts();
        assert_eq!(Url::from_parts(parts).unwrap(), u, "{src:?}");
        let joined = url("http://a/b/c?q").join_url(&u);
        Url::validate(joined.as_str()).unwrap_or_else(|e| panic!("{src:?} -> {joined:?}: {e}"));
    }
}
