# Changes

## [1.1.0] - 2026-10-05

* `Url::parse()` parses authority-form `[userinfo@]host[:port]` like `http::Uri`, the
  previous `Url::parse()` is renamed to `Url::parse_ref()`

* `http::Uri` conversions keep the authority of authority-form, a network-path reference
  converts to authority-form and fails if it has a path or query

* Add `Authority::host_port()`

## [1.0.1] - 2026-10-05

* Add `const fn Url::new()`, same as `Url::default()`

* Add `+` and `+=` operators for `Url`, same as `Url::join_url()`

## [1.0.0] - 2026-10-04

* Refine api and tests

## [0.1.0] - 2026-10-04

* Initial release
