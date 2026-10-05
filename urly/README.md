# urly

Yet another URL library for [ntex](https://github.com/ntex-rs/ntex). The API is
inspired by Python's [yarl](https://yarl.aio-libs.org/).

[![Crates.io][crates-badge]][crates-url]

[crates-badge]: https://img.shields.io/crates/v/urly.svg
[crates-url]: https://crates.io/crates/urly

[Documentation](https://docs.rs/urly)

`Url` is an immutable, cheaply cloneable, normalized URL reference. It is stored
in a single `ByteString`, and parsing reuses the input buffer when the input is
already normalized.

* Lenient parsing with normalization: percent-encoding, IDNA hosts, dot
  segments and case. `Url::validate` checks strictly against RFC 3986.
* Borrowed, percent-encoded accessors (`Scheme`, `Authority`, `Path`, `Query`,
  `Fragment`) with methods to decode them.
* Path helpers: `/` operator, `join`, `parent`, `file_name`, `extension`.
* Query helpers: `get`, `get_all`, `set_query_pairs`, `extend_query_pairs`,
  `update_query_pairs`, `remove_query_params`.
* `Builder` and `Parts` for constructing URLs from components.
* `quote`, `requote` and `unquote` per URL component in `urly::quoting`.

## Usage

```toml
[dependencies]
urly = "0.1"
```

```rust
use urly::Url;

let url = Url::from_static("https://example.com/api/v1?page=2");
assert_eq!(url.host(), Some("example.com"));
assert_eq!(url.port_or_known_default(), Some(443));

let users = &url / "users" / "john doe";
assert_eq!(users, "https://example.com/api/v1/users/john%20doe");

let mut url = users.join("../groups?id=1").unwrap();
url.extend_query_pairs([("sort", "name asc")]);
assert_eq!(url, "https://example.com/api/v1/groups?id=1&sort=name+asc");

let url = Url::builder()
    .scheme("http")
    .host("münchen.de")
    .path("/a b")
    .query_pair("q", "x&y")
    .build()
    .unwrap();
assert_eq!(url, "http://xn--mnchen-3ya.de/a%20b?q=x%26y");
```

## Requoting

Parsing requotes the input instead of encoding it again, so an already-encoded
URL is normalized and parsing a normalized URL returns it unchanged:

* valid `%XX` escapes are kept and their hex digits are uppercased
* escapes of unreserved characters (`A-Z a-z 0-9 - . _ ~`) are decoded
* `%2F` in a path is kept encoded, so it doesn't become a path separator
* any other character that isn't allowed in the component is percent-encoded,
  including non-ASCII characters (as UTF-8) and a `%` that doesn't start a
  valid escape
* a space is encoded as `%20`, and as `+` in the query

Accessors return the percent-encoded component; use `decode` for the plain
value.

```rust
use urly::Url;

let url = Url::parse("http://h/a b/%7euser/100%/x%2fy/€?q=a b&r=%41%3d").unwrap();
assert_eq!(url, "http://h/a%20b/~user/100%25/x%2Fy/%E2%82%AC?q=a+b&r=A%3D");
assert_eq!(url.path().decode(), "/a b/~user/100%/x/y/€");
assert_eq!(url.query().unwrap().get("q").unwrap(), "a b");
```

`urly::quoting` applies the same rules to a single value: `quote` encodes a
literal value, `requote` normalizes an encoded one and `unquote` decodes it.

```rust
use urly::quoting::{Component, quote, requote, unquote};

assert_eq!(quote("a b/100%", Component::Path), "a%20b/100%25");
assert_eq!(requote("a b/100%25%7e", Component::Path), "a%20b/100%25~");
assert_eq!(unquote("a+b%26c", Component::QueryPart), "a b&c");
```

## Features

* `http` - conversions between `Url` and `http::Uri`
* `serde` - `Serialize` and `Deserialize` for `Url`

```toml
[dependencies]
urly = { version = "0.1", features = ["http", "serde"] }
```

## License

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
  [http://www.apache.org/licenses/LICENSE-2.0])
* MIT license ([LICENSE-MIT](LICENSE-MIT) or
  [http://opensource.org/licenses/MIT])
