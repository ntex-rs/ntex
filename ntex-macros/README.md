# ntex-macros

[![crates.io](https://img.shields.io/crates/v/ntex-macros.svg)](https://crates.io/crates/ntex-macros)
[![Documentation](https://docs.rs/ntex-macros/badge.svg)](https://docs.rs/ntex-macros)

Procedural macros for [ntex](https://github.com/ntex-rs/ntex).

You don't need to add this crate to your dependencies, `ntex` re-exports
all of its macros:

- `#[ntex::main]` runs an async `main` on the ntex runtime.
- `#[ntex::test]` runs an async test on the ntex runtime.
- `#[ntex::web::get]`, `post`, `put`, `delete`, `head`, `connect`, `options`,
  `trace`, `patch` and `query` turn an async function into a web handler.

```rust
use ntex::{SharedCfg, web};

#[web::get("/")]
async fn index() -> &'static str {
    "Hello world!"
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::HttpServer::new(async |_| web::App::new().service(index))
        .bind("127.0.0.1:8080", SharedCfg::new("hello-world"))?
        .run()
        .await
}
```

See the [API docs](https://docs.rs/ntex-macros) for all macro arguments.

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE))
- MIT license ([LICENSE-MIT](../LICENSE-MIT))

at your option.
