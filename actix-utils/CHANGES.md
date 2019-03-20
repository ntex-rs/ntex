# Changes

### Added

* Allow to send messages to `FramedTransport` via mpsc channel.


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
