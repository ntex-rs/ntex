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
* Percent-encoding with Python yarl rules in `urly::quoting`.

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
