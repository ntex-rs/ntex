# Changes

## [2.2.0] - 2024-08-12

* Allow to notify dispatcher from IoRef

## [2.1.0] - 2024-07-30

* Optimize `Io` layout

## [2.0.0] - 2024-05-28

* Use async fn for Service::ready() and Service::shutdown()

## [1.2.0] - 2024-05-12

* Better write back-pressure handling

* Dispatcher optimization for handling first item

## [1.1.0] - 2024-05-01

* Add IoRef::notify_timeout() helper method

* Fix KeepAlive timeout handling in default dispatcher

## [1.0.2] - 2024-03-31

* Add IoRef::is_wr_backpressure() method

## [1.0.1] - 2024-02-05

* Add IoBoxed::take() method

## [1.0.0] - 2024-01-09

* Release

## [1.0.0-b.1] - 2024-01-08

* Remove FilterFactory trait and related utils

## [1.0.0-b.0] - 2024-01-07

* Use "async fn" in trait for Service definition

* Min timeout more than 1sec

## [0.3.17] - 2023-12-25

* Fix filter leak during Io drop

## [0.3.16] - 2023-12-14

* Better io tags handling

## [0.3.15] - 2023-12-12

* Add io tags for logging

* Stop dispatcher timers on memory pool pause

## [0.3.14] - 2023-12-10

* Fix KEEP-ALIVE timer handling

## [0.3.13] - 2023-12-02

* Optimize KEEP-ALIVE timer

## [0.3.12] - 2023-11-29

* Refactor io timers

* Tune logging

## [0.3.11] - 2023-11-25

* Fix keep-alive timeout handling

## [0.3.10] - 2023-11-23

* Refactor slow frame timeout handling

## [0.3.9] - 2023-11-21

* Remove slow frame timer if service is not ready

* Do not process data in Dispatcher from read buffer after disconnect

## [0.3.8] - 2023-11-17

* Remove useless logs

## [0.3.7] - 2023-11-12

* Handle io flush during write back-pressure

## [0.3.6] - 2023-11-11

* Add support for frame read timeout

* Add DispatcherConfig type

## [0.3.5] - 2023-11-03

* Add Io::force_ready_ready() and Io::poll_force_ready_ready() methods

## [0.3.3] - 2023-09-11

* Add missing fmt::Debug impls

## [0.3.2] - 2023-08-10

* Replace `PipelineCall` with `ServiceCall<'static, S, R>`

## [0.3.1] - 2023-06-23

* `PipelineCall` is static

## [0.3.0] - 2023-06-22

* Release v0.3.0

## [0.3.0-beta.3] - 2023-06-21

* Use static ContainerCall for dispatcher

## [0.3.0-beta.0] - 2023-06-16

* Migrate to ntex-service 1.2

## [0.2.10] - 2023-05-10

* ReadBuf::set_dst()/WriteBuf::set_dst() extend existing buffer if exists

## [0.2.9] - 2023-01-31

* Register Dispatcher waker when service is not ready

* Add Io::poll_read_pause() method, pauses read task and check io status

## [0.2.8] - 2023-01-30

* Check for nested io operations

## [0.2.7] - 2023-01-29

* Refactor buffer api

## [0.2.6] - 2023-01-27

* Add IoRef::with_rw_buf() helper

## [0.2.5] - 2023-01-27

* Custom panic message for nested buffer borrow

## [0.2.4] - 2023-01-26

* Refactor write task management

## [0.2.3] - 2023-01-25

* Optimize buffers layout

* Release empty buffers

## [0.2.2] - 2023-01-24

* Process write buffer if filter wrote to write buffer during reading

## [0.2.1] - 2023-01-23

* Refactor Io and Filter types

## [0.2.0] - 2023-01-04

* Release

## [0.2.0-beta.0] - 2022-12-28

* Upgrade to ntex-service 1.0

* Restart timer after runtime stop

## [0.1.11] - 2022-12-02

* Expose IoRef::start_keepalive_timer() and IoRef::remove_keepalive_timer() methods

## [0.1.10] - 2022-10-31

* Fix compilation errors in the openwrt environment #140

## [0.1.9] - 2022-10-03

* Fix on-disconnect never resolving #135

## [0.1.8] - 2022-02-19

* Add HttpProtocol type from ntex-tls

## [0.1.7] - 2022-01-30

* Use BytesVec type for buffers and Filter trait

## [0.1.6] - 2022-01-27

* Optimize Io memory layout

## [0.1.5] - 2022-01-23

* Add Eq,PartialEq,Hash,Debug impls to Io asn IoRef

## [0.1.4] - 2022-01-17

* Add Io::take() method

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
