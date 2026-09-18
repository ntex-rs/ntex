<div align="center">
  <h1>ntex</h1>
  <p><strong>Framework for composable network services.</strong></p>

[![Linux](https://github.com/ntex-rs/ntex/actions/workflows/linux.yml/badge.svg?branch=main)](https://github.com/ntex-rs/ntex/actions/workflows/linux.yml)
[![crates.io](https://img.shields.io/crates/v/ntex.svg)](https://crates.io/crates/ntex)
[![Documentation](https://img.shields.io/docsrs/ntex/latest)](https://docs.rs/ntex)
[![MSRV](https://img.shields.io/badge/rustc-1.97+-lightgray.svg)](https://releases.rs/docs/1.97.0/)
[![License](https://img.shields.io/crates/l/ntex.svg)](https://github.com/ntex-rs/ntex#license)
[![codecov](https://codecov.io/gh/ntex-rs/ntex/branch/main/graph/badge.svg)](https://codecov.io/gh/ntex-rs/ntex/tree/main)
[![Discord](https://img.shields.io/discord/919288597826387979?label=chat&logo=discord)](https://discord.gg/4GtaeP5Uqu)
</div>

ntex provides a strongly typed service and middleware model for building
asynchronous network applications. It includes HTTP/1, HTTP/2, WebSocket,
Mqtt3/5, Amqp1.0, TLS, and runtime-independent I/O support.

## Build status

| Platform | Status |
| --- | --- |
| Linux | [![Linux](https://github.com/ntex-rs/ntex/actions/workflows/linux.yml/badge.svg?branch=main)](https://github.com/ntex-rs/ntex/actions/workflows/linux.yml) |
| macOS | [![macOS](https://github.com/ntex-rs/ntex/actions/workflows/osx.yml/badge.svg?branch=main)](https://github.com/ntex-rs/ntex/actions/workflows/osx.yml) |
| Windows | [![Windows](https://github.com/ntex-rs/ntex/actions/workflows/windows.yml/badge.svg?branch=main)](https://github.com/ntex-rs/ntex/actions/workflows/windows.yml) |

## Usage

Add ntex to your project:

```toml
[dependencies]
ntex = "4"
```

A minimal web server looks like this:

```rust
use ntex::{SharedCfg, web},
};

#[web::get("/")]
async fn index() -> &'static str {
    "Hello world!"
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::HttpServer::new(async |_| wev::App::new().service(index))
        .bind("127.0.0.1:8080", SharedCfg::new("hello-world"))?
        .run()
        .await
}
```

## Runtime selection

Without an explicit runtime feature, ntex uses its native single-threaded
runtime and automatically selects a platform I/O reactor:

- Linux tries `io_uring` and falls back to polling.
- Other Unix platforms use polling.
- Windows uses IOCP.

Alternative runtimes and native reactors can be selected with Cargo features:

| Feature | Runtime or reactor |
| --- | --- |
| `tokio` | Tokio local runtime and I/O driver |
| `compio` | Compio runtime and completion-based I/O |
| `neon-polling` | Native runtime with the polling reactor |
| `neon-uring` | Native runtime with `io_uring` on Linux |
| `neon-iocp` | Native runtime with IOCP on Windows |

Enable at most one runtime or native-reactor selection feature. For example:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio"] }
```

## Optional features

- `openssl` and `rustls` enable the corresponding TLS integrations.
- `compress` enables HTTP content compression.
- `cookie` enables cookie support.
- `url` enables URL parsing support.
- `ws` enables WebSocket APIs and is enabled by default.

## Documentation and community

- [Framework guide](https://github.com/ntex-rs/ntex/tree/main/docs)
- [Web framework documentation](https://ntex.rs)
- [API documentation](https://docs.rs/ntex)
- [Examples](https://github.com/ntex-rs/examples)
- [Release changes](https://github.com/ntex-rs/ntex/blob/main/ntex/CHANGES.md)
- Minimum supported Rust version: 1.97

## License

This project is licensed under either of:

- Apache License, Version 2.0
  ([LICENSE-APACHE](https://github.com/ntex-rs/ntex/blob/main/LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license
  ([LICENSE-MIT](https://github.com/ntex-rs/ntex/blob/main/LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)
