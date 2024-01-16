# Bytes

A utility library for working with bytes. This is fork of bytes crate (https://github.com/tokio-rs/bytes)

[![Crates.io][crates-badge]][crates-url]

[crates-badge]: https://img.shields.io/crates/v/ntex-bytes.svg
[crates-url]: https://crates.io/crates/ntex-bytes

[Documentation](https://docs.rs/ntex-bytes)

## Usage

To use `ntex-bytes`, first add this to your `Cargo.toml`:

```toml
[dependencies]
ntex-bytes = "0.1"
```

Next, add this to your crate:

```rust
use ntex_bytes::{Bytes, BytesMut, Buf, BufMut};
```

## Serde support

Serde support is optional and disabled by default. To enable use the feature `serde`.

```toml
[dependencies]
ntex-bytes = { version = "0.1", features = ["serde"] }
```

## License

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
  [http://www.apache.org/licenses/LICENSE-2.0])
* MIT license ([LICENSE-MIT](LICENSE-MIT) or
  [http://opensource.org/licenses/MIT])
