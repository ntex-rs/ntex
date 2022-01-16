<div align="center">
 <p><h1>ntex</h1> </p>
  <p><strong>Framework for composable network services.</strong> </p>
  <p>

[![build status](https://github.com/ntex-rs/ntex/workflows/CI%20%28Linux%29/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions?query=workflow%3A"CI+(Linux)") 
[![codecov](https://codecov.io/gh/ntex-rs/ntex/branch/master/graph/badge.svg)](https://codecov.io/gh/ntex-rs/ntex) 
[![crates.io](https://img.shields.io/crates/v/ntex.svg)](https://crates.io/crates/ntex) 
[![Documentation](https://docs.rs/ntex/badge.svg)](https://docs.rs/ntex) 
[![Version](https://img.shields.io/badge/rustc-1.53+-lightgray.svg)](https://blog.rust-lang.org/2021/06/17/Rust-1.53.0.html) 
![License](https://img.shields.io/crates/l/ntex.svg) 

  </p>
</div>

## Build statuses

| Platform         | Build Status |
| ---------------- | ------------ |
| Linux            | [![build status](https://github.com/ntex-rs/ntex/workflows/CI%20%28Linux%29/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions?query=workflow%3A"CI+(Linux)") |
| macOS            | [![build status](https://github.com/ntex-rs/ntex/workflows/CI%20%28OSX%29/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions?query=workflow%3A"CI+(OSX)") |
| Windows          | [![build status](https://github.com/ntex-rs/ntex/workflows/CI%20%28Windows%29/badge.svg?branch=master&event=push)](https://github.com/ntex-rs/ntex/actions?query=workflow%3A"CI+(Windows)") |

## Usage

Starting ntex v0.5 async runtime must be selected as a feature. Available options are `glommio`,
`tokio` or `async-std`.

```toml
[dependencies]
ntex = { version = "0.5", features = ["glommio"] }
```

## Documentation & community resources

* [Documentation](https://docs.rs/ntex)
* Minimum supported Rust version: 1.53 or later

## License

This project is licensed under

* MIT license ([LICENSE](LICENSE) or [http://opensource.org/licenses/MIT](http://opensource.org/licenses/MIT))
