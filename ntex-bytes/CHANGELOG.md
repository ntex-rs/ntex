# Changes

## [0.1.8] (2021-12-18)

* Remove futures patch dependency

## [0.1.7] (2021-12-06)

* Fix dealloc for vec representation

## [0.1.6] (2021-12-03)

* Better api usability

## [0.1.5] (2021-12-02)

* Split,freeze,truncate operations produce inline Bytes object if possible

* Refactor Vec representation

* Introduce memory pools

## [0.1.4] (2021-06-27)

* Reduce size of Option<Bytes> by using NonNull

## [0.1.2] (2021-06-27)

* Reserve space for put_slice

## [0.1.1] (2021-06-27)

* Add `ByteString::as_slice()` method

* Enable serde

## [0.1.0] (2021-06-27)

* Add `Bytes::trimdown()` method

* Add `ByteString::slice()`, `ByteString::slice_off()`, `ByteString::slice_to()`

* Remove unused code

* Project fork from 0.4 version
