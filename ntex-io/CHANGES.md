# Changes

## [0.1.3] - 2022-01-12

* Refactor Filter trait, fix read buffer processing

## [0.1.2] - 2022-01-10

* Remove unneeded boxed types

* Add Framed::into_inner() helper method

## [0.1.1] - 2022-01-03

* Move tokio support to separate crate

* Move async-std support to separate crate

## [0.1.0] - 2021-12-30

* Unify keep-alive timers

* Add Io::poll_status_update() method to use instead of register_dispatcher()

* Reset DSP_STOP and DSP_KEEPALIVE flags

## [0.1.0-b.10] - 2021-12-30

* IoRef::close() method initiates io stream shutdown

* IoRef::force_close() method terminates io stream

* Cleanup Filter trait, removed closed,want_read,want_shutdown methods

* Cleanup internal flags on io error

## [0.1.0-b.9] - 2021-12-29

* Add `async-std` support

## [0.1.0-b.8] - 2021-12-28

* Fix error handing for nested filters

* Improve tokio streams support

## [0.1.0-b.7] - 2021-12-27

* Do not swallow decoded read bytes in case of filter error

## [0.1.0-b.6] - 2021-12-26

* Rename `RecvError::StopDispatcher` to `RecvError::Stop`

* Better error information for .poll_recv() method.

* Remove redundant Io::poll_write_backpressure() method.

* Add Framed type

* Fix read filters ordering

* Fix read filter root buffer

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
