# Changes

## [0.1.0-b.3] - 2021-12-xx

* Rename .poll_write_ready() to .poll_flush()

* Rename .write_ready() to .flush()

## [0.1.0-b.2] - 2021-12-20

* Removed `WriteRef` and `ReadRef`

* Better Io/IoRef api separation

* DefaultFilter renamed to Base

## [0.1.0-b.1] - 2021-12-19

* Remove ReadFilter/WriteFilter traits.

## [0.1.0-b.0] - 2021-12-18

* Refactor ntex::framed to ntex-io
