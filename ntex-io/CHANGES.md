# Changes

## [0.1.0-b.6] - 2021-12-xx

* Fix read filters ordering

## [0.1.0-b.5] - 2021-12-24

* Use new ntex-service traits

* Make `IoBoxed` into spearate type

* Add `SealedService` and `SealedFactory` helpers

## [0.1.0-b.4] - 2021-12-23

* Introduce `Sealed` type instead of `Box<dyn Filter>`

## [0.1.0-b.3] - 2021-12-22

* Add .poll_write_backpressure()

* Rename .poll_read_next() to .poll_recv()

* Rename .poll_write_ready() to .poll_flush()

* Rename .next() to .recv()

* Rename .write_ready() to .flush()

* .poll_read_ready() cleanups RD_PAUSED state

## [0.1.0-b.2] - 2021-12-20

* Removed `WriteRef` and `ReadRef`

* Better Io/IoRef api separation

* DefaultFilter renamed to Base

## [0.1.0-b.1] - 2021-12-19

* Remove ReadFilter/WriteFilter traits.

## [0.1.0-b.0] - 2021-12-18

* Refactor ntex::framed to ntex-io
