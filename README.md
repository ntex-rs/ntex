<div align="center">
 <p><h1>ntex</h1> </p>
  <p><strong>Framework for composable network services.</strong> </p>
  <p>

[![build status](https://github.com/ntex-rs/ntex/actions/workflows/linux.yml/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions/workflows/linux.yml/badge.svg) 
[![crates.io](https://img.shields.io/crates/v/ntex.svg)](https://crates.io/crates/ntex) 
[![Documentation](https://img.shields.io/docsrs/ntex/latest)](https://docs.rs/ntex) 
[![Version](https://img.shields.io/badge/rustc-1.83+-lightgray.svg)](https://releases.rs/docs/1.83.0/) 
![License](https://img.shields.io/crates/l/ntex.svg) 
[![codecov](https://codecov.io/gh/ntex-rs/ntex/branch/master/graph/badge.svg)](https://codecov.io/gh/ntex-rs/ntex) 
[![Chat on Discord](https://img.shields.io/discord/919288597826387979?label=chat&logo=discord)](https://discord.gg/4GtaeP5Uqu) 
 
  </p>
</div>

## Build statuses

| Platform         | Build Status |
| ---------------- | ------------ |
| Linux            | [![build status](https://github.com/ntex-rs/ntex/actions/workflows/linux.yml/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions/workflows/linux.yml/badge.svg) |
| macOS            | [![build status](https://github.com/ntex-rs/ntex/actions/workflows/osx.yml/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions/workflows/osx.yml/badge.svg) |
| Windows          | [![build status](https://github.com/ntex-rs/ntex/actions/workflows/windows.yml/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions/workflows/windows.yml/badge.svg) |

## Usage

ntex supports multiple async runtimes, runtime must be selected as a feature. Available options are `compio`, `tokio`,
`neon` or `neon-uring`.

```toml
[dependencies]
ntex = { version = "2", features = ["compio"] }
```

## Documentation & community resources

* [Documentation](https://ntex.rs)
* [Docs.rs](https://docs.rs/ntex)
* Minimum supported Rust version: 1.83 or later

## License

This project is licensed under

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
  [http://www.apache.org/licenses/LICENSE-2.0])
* MIT license ([LICENSE-MIT](LICENSE-MIT) or
  [http://opensource.org/licenses/MIT])
