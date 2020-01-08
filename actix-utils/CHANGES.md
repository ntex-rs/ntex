# Changes

## [1.0.6] - 2020-01-08

* Add `Clone` impl for `condition::Waiter`

## [1.0.5] - 2020-01-08

* Add `Condition` type.

* Add `Pool` of one-shot's.

## [1.0.4] - 2019-12-20

* Add methods to check `LocalWaker` registration state.

## [1.0.3] - 2019-12-11

* Revert InOrder service changes

## [1.0.2] - 2019-12-11

* Allow to create `framed::Dispatcher` with custom `mpsc::Receiver`

* Add `oneshot::Sender::is_canceled()` method

## [1.0.1] - 2019-12-11

* Optimize InOrder service

## [1.0.0] - 2019-12-11

* Simplify oneshot and mpsc implementations

## [1.0.0-alpha.3] - 2019-12-07

* Migrate to tokio 0.2

* Fix oneshot

## [1.0.0-alpha.2] - 2019-12-02

* Migrate to `std::future`

## [0.4.7] - 2019-10-14

* Re-register task on every framed transport poll.


## [0.4.6] - 2019-10-08

* Refactor `Counter` type. register current task in available method.


## [0.4.5] - 2019-07-19

### Removed

* Deprecated `CloneableService` as it is not safe


## [0.4.4] - 2019-07-17

### Changed

* Undeprecate `FramedTransport` as it is actually useful


## [0.4.3] - 2019-07-17

### Deprecated

* Deprecate `CloneableService` as it is not safe and in general not very useful

* Deprecate `FramedTransport` in favor of `actix-ioframe`


## [0.4.2] - 2019-06-26

### Fixed

* Do not block on sink drop for FramedTransport


## [0.4.1] - 2019-05-15

### Changed

* Change `Either` constructor


## [0.4.0] - 2019-05-11

### Changed

* Change `Either` to handle two nexted services

* Upgrade actix-service 0.4

### Deleted

* Framed related services

* Stream related services

## [0.3.5] - 2019-04-04

### Added

* Allow to send messages to `FramedTransport` via mpsc channel.

### Changed

* Remove 'static constraint from Clonable service


## [0.3.4] - 2019-03-12

### Changed

* `TimeoutService`, `InOrderService`, `InFlightService` accepts generic IntoService services.

### Fixed

* Fix `InFlightService::poll_ready()` nested service readiness check

* Fix `InOrderService::poll_ready()` nested service readiness check


## [0.3.3] - 2019-03-09

### Changed

* Revert IntoFuture change

* Add generic config param for IntoFramed and TakeOne new services


## [0.3.2] - 2019-03-04

### Changed

* Use IntoFuture for new services


## [0.3.1] - 2019-03-04

### Changed

* Use new type of transform trait


## [0.3.0] - 2019-03-02

### Changed

* Use new `NewService` trait

* BoxedNewService` and `BoxedService` types moved to actix-service crate.


## [0.2.4] - 2019-02-21

### Changed

* Custom `BoxedNewService` implementation.


## [0.2.3] - 2019-02-21

### Added

* Add `BoxedNewService` and `BoxedService`


## [0.2.2] - 2019-02-11

### Added

* Add `Display` impl for `TimeoutError`

* Add `Display` impl for `InOrderError`


## [0.2.1] - 2019-02-06

### Added

* Add `InOrder` service. the service yields responses as they become available,
  in the order that their originating requests were submitted to the service.

### Changed

* Convert `Timeout` and `InFlight` services to a transforms


## [0.2.0] - 2019-02-01

* Fix framed transport error handling

* Added Clone impl for Either service

* Added Clone impl for Timeout service factory

* Added Service and NewService for Stream dispatcher

* Switch to actix-service 0.2


## [0.1.0] - 2018-12-09

* Move utils services to separate crate
