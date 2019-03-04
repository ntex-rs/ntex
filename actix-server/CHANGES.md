# Changes

## [0.3.1] - 2019-03-xx

### Added

* Add `ServerBuilder::maxconnrate` sets the maximum per-worker number of concurrent connections


### Changed

* Rename `StreamServiceFactory` to `ServiceFactory`

* Deprecate `StreamServiceFactory`


## [0.3.0] - 2019-03-02

### Changed

* Use new `NewService` trait


## [0.2.1] - 2019-02-09

### Changed

* Drop service response


## [0.2.0] - 2019-02-01

### Changed

* Migrate to actix-service 0.2

* Updated rustls dependency


## [0.1.3] - 2018-12-21

### Fixed

* Fix max concurrent connections handling


## [0.1.2] - 2018-12-12

### Changed

* rename ServiceConfig::rt() to ServiceConfig::apply()


### Fixed

* Fix back-pressure for concurrent ssl handshakes


## [0.1.1] - 2018-12-11

* Fix signal handling on windows


## [0.1.0] - 2018-12-09

* Move server to separate crate
