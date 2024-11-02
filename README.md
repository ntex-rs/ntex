<div align="center">
 <p><h1>ntex</h1> </p>
  <p><strong>Framework for composable network services.</strong> </p>
  <p>

[![build status](https://github.com/ntex-rs/ntex/workflows/CI%20%28Linux%29/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions?query=workflow%3A"CI+(Linux)") 
[![crates.io](https://img.shields.io/crates/v/ntex.svg)](https://crates.io/crates/ntex) 
[![Documentation](https://img.shields.io/docsrs/ntex/latest)](https://docs.rs/ntex) 
[![Version](https://img.shields.io/badge/rustc-1.75+-lightgray.svg)](https://blog.rust-lang.org/2023/12/28/Rust-1.75.0.html) 
![License](https://img.shields.io/crates/l/ntex.svg) 
[![codecov](https://codecov.io/gh/ntex-rs/ntex/branch/master/graph/badge.svg)](https://codecov.io/gh/ntex-rs/ntex) 
[![Chat on Discord](https://img.shields.io/discord/919288597826387979?label=chat&logo=discord)](https://discord.com/channels/919288597826387979/919288597826387982) 
 
  </p>
</div>

## Build statuses

| Platform         | Build Status |
| ---------------- | ------------ |
| Linux            | [![build status](https://github.com/ntex-rs/ntex/workflows/CI%20%28Linux%29/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions?query=workflow%3A"CI+(Linux)") |
| macOS            | [![build status](https://github.com/ntex-rs/ntex/workflows/CI%20%28OSX%29/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions?query=workflow%3A"CI+(OSX)") |
| Windows          | [![build status](https://github.com/ntex-rs/ntex/workflows/CI%20%28Windows%29/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions?query=workflow%3A"CI+(Windows)") |

## Usage

ntex supports multiple async runtimes, runtime must be selected as a feature. Available options are `compio`, `tokio`,
`glommio` or `async-std`.

```toml
[dependencies]
ntex = { version = "2", features = ["tokio"] }
```

## Documentation & community resources

* [Documentation](https://ntex.rs)
* [Docs.rs](https://docs.rs/ntex)
* Minimum supported Rust version: 1.75 or later

## License

This project is licensed under

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
  [http://www.apache.org/licenses/LICENSE-2.0])
* MIT license ([LICENSE-MIT](LICENSE-MIT) or
  [http://opensource.org/licenses/MIT])
