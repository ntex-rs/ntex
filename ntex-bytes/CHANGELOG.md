# Changes

## [1.0.0] (2025-11-24)

* Drop memory pool support

## [0.1.31] (2025-11-15)

* Refactor code structure

## [0.1.30] (2025-06-26)

* Add helper PartialEq to ByteString

## [0.1.29] (2025-06-26)

* Add From<&ByteSting> for ByteString impl

## [0.1.28] (2025-04-28)

* Reclaim BytesVec on truncate

## [0.1.27] (2024-04-08)

* Move more code under mpool feature

## [0.1.26] (2024-04-04)

* Make memory pools optional

## [0.1.25] (2024-04-02)

* Fix pool waiters management

## [0.1.24] (2024-02-01)

* Add `checked` api

## [0.1.23] (2024-01-19)

* Revert changes to release shared_vec

## [0.1.22] (2024-01-16)

* Use const generics for helper traits (From, PartialEq, PartialOrd)

## [0.1.21] (2023-11-1)

* Data race in BytesMut #243

## [0.1.19] (2023-01-23)

* Add PollRef::resize_read_buf() and PollRef::resize_write_buf() helpers

## [0.1.18] (2022-12-13)

* Add Bytes<&Bytes> for Bytes impl

* Remove ByteString::TryFrom<&BytesMut> impl

## [0.1.17] (2022-12-09)

* Add TryFrom<&Bytes> and TryFrom<&BytesMut> for ByteString

## [0.1.16] (2022-07-07)

* Add ByteString::clear() method

## [0.1.15] (2022-06-20)

* Add Buf/BufMut impls

* Remove .assume_init usage #106

## [0.1.14] (2022-02-06)

* Restore Buf impl for Cursor

## [0.1.13] (2022-02-04)

* Remove unused impls for BytesMut and BytesVec

* Add some tests for BytesVec #102

## [0.1.12] (2022-01-31)

* Fix conversion from BytesVec to BytesMut and back (BytesVec::with_bytes_mut())

## [0.1.11] (2022-01-30)

* Add BytesVec type

## [0.1.10] (2022-01-26)

* Rename Pool::is_pending() to is_ready()

* Use u32 instead of u16 for read/write params

## [0.1.9] (2022-01-10)

* Add optional simd utf8 validation

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
