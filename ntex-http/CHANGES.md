# Changes

## [0.1.12] - 2024-01-16

* Update http dependency

## [0.1.11] - 2023-11-21

* Implement serde::Serialize/Deserialize for HeaderMap

## [0.1.10] - 2023-09-11

* Add missing fmt::Debug impls

## [0.1.9] - 2022-12-09

* Add helper method HeaderValue::as_shared()

## [0.1.8] - 2022-11-30

* Convert from HeaderValue into http::header::HeaderValue

## [0.1.7] - 2022-11-24

* Keep multi header values order #145

## [0.1.6] - 2022-11-10

* Add From<HeaderValue> impl for Value, allow to use HeaderMap::collect()

## [0.1.5] - 2022-11-04

* Add ByteString to HeaderValue conversion support

## [0.1.4] - 2022-11-03

* Add http::Error to Error

## [0.1.3] - 2022-11-03

* Use custom `Error` and `HeaderValue` types

## [0.1.2] - 2022-11-01

* Re-export http::Error

## [0.1.1] - 2022-06-26

* impl PartialEq for HeaderMap

* impl TryFrom<&str> for Value

* impl FromIterator for HeaderMap

## [0.1.0] - 2022-06-20

* Move http types to separate crate
