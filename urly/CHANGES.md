# Changes

## [1.4.0] - 2026-11-12

* Add `Url::parse_target_str()`, `Url::parse_target()` without the UTF-8 check
  for targets already known to be UTF-8, taking the query position found by
  the caller's parser

## [1.3.1] - 2026-11-11

* `Url::parse_target()` builds origin-form targets such as `/a?b` directly
  instead of reassembling them, about twice as fast

## [1.3.0] - 2026-11-09

* Requoting decodes only unreserved escapes (`A-Z a-z 0-9 - . _ ~`); escapes of
  reserved characters such as `%2F` and `%3B` are kept, so decoding can no longer
  change how a path or query is split

* Normalization removes the scheme's default port, so `https://h:443/` and
  `https://h/` are equal. `Url::parse_target()` keeps the port as received

* `Url::push_segments()` and `Url / segment` resolve `.` and `..` only within the
  appended segments, so they can no longer escape the base path

* Return `ErrorKind::TooLong` from `Builder::build` instead of panicking when
  the path, query or fragment makes the URL longer than 65535 bytes

## [1.2.1] - 2026-11-03

* Improve quoting performance

## [1.2.0] - 2026-10-31

* Add `Url::parse_target()` and `Url::from_target_parts()` to preserve unvalidated
  HTTP request-target components, `Url::validate_target()` for explicit validation,
  and `Url::normalize()` for in-place normalization. General-purpose parsing
  continues to normalize by default.

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
