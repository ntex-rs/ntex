# Changes

## [2.2.0] - 2024-07-30

* Add LocalWaker::with() helper

## [2.1.0] - 2024-06-26

* Add task::yield_to() helper

## [2.0.1] - 2024-05-28

* Re-enable BufferService

## [2.0.0] - 2024-05-28

* Use async fn for Service::ready() and Service::shutdown()

## [1.1.0] - 2024-04-xx

* Change Extensions::insert() method according doc #345

## [1.0.1] - 2024-01-19

* Allow to lock readiness for Condition

## [1.0.0] - 2024-01-09

* Release

## [1.0.0-b.1] - 2024-01-08

* Remove unnecessary 'static

## [1.0.0-b.0] - 2024-01-07

* Use "async fn" in trait for Service definition

## [0.3.4] - 2023-11-06

* Add UnwindSafe trait on mpsc::Receiver<T> #239

## [0.3.3] - 2023-11-02

* Add FusedStream trait on mpsc::Receiver<T> #235

## [0.3.2] - 2023-09-11

* Add missing fmt::Debug impls

## [0.3.1] - 2023-06-24

* Changed `BufferService` to maintain order

* Buffer error type changed to indicate cancellation

## [0.3.0] - 2023-06-22

* Release v0.3.0

## [0.3.0-beta.0] - 2023-06-16

* Upgrade to ntex-service 1.2

* Remove unneeded SharedService

## [0.2.3] - 2023-06-04

* Refactor timer driver

## [0.2.2] - 2023-04-20

* Add OneRequest service, service that allows to handle one request at time

## [0.2.1] - 2023-04-14

* Add SharedService, a service that can be checked for readiness by multiple tasks

## [0.2.0] - 2023-01-04

* Release

## [0.2.0-beta.0] - 2022-12-28

* Migrate to ntex-service 1.0

## [0.1.19] - 2022-12-13

* Add `BoxFuture` helper type alias

## [0.1.18] - 2022-11-25

* Add Extensions::extend() and Extensions::is_empty() methods

* Add fmt::Debug impl to channel::Pool

## [0.1.17] - 2022-05-25

* Allow to reset time::Deadline

## [0.1.16] - 2022-02-19

* Add time::Deadline future

## [0.1.15] - 2022-02-18

* Fix update timer handle with 0 millis, do not keep old bucket

## [0.1.14] - 2022-02-18

* time::sleep() always sleeps one tick (16 millis) even for 0 millis

## [0.1.13] - 2022-01-28

* Add Default impl to oneshots pool

## [0.1.12] - 2022-01-27

* Reduce size of Millis

## [0.1.11] - 2022-01-23

* Remove useless stream::Dispatcher and sink::SinkService

## [0.1.10] - 2022-01-17

* Add time::query_system_time(), it does not use async runtime

## [0.1.9] - 2022-01-12

* Add Pool::shrink_to_fit() method

## [0.1.8] - 2022-01-10

* Add pool::Receiver::poll_recv() method

* Add oneshot::Receiver::poll_recv() method

## [0.1.7] - 2022-01-04

* Add time::timeout_checked, if duration is zero then timeout is disabled

## [0.1.6] - 2022-01-03

* Use ntex-rt::spawn

* Move ntex::util services

## [0.1.5] - 2021-12-27

* Fix borrow error when timer get dropped immidietly after start

## [0.1.4] - 2021-12-21

* mpsc: add Receiver::poll_recv() method

## [0.1.3] - 2021-12-18

* move ntex::channel::mpsc

## [0.1.2] - 2021-12-10

* move in ntex::time utils

* replace tokio::time with futures-timer

## [0.1.1] - 2021-04-11

* next renamed to stream_recv

* send renamed to sink_write

## [0.1.0] - 2021-04-04

* Move utils to separate crate
