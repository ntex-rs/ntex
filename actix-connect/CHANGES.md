# Changes

## [0.2.1] - 2019-07-17

### Added

* Expose Connect addrs #30

### Changed

* Update `derive_more` to 0.15


## [0.2.0] - 2019-05-12

### Changed

* Upgrade to actix-service 0.4


## [0.1.5] - 2019-04-19

### Added

* `Connect::set_addr()`

### Changed

* Use trust-dns-resolver 0.11.0


## [0.1.4] - 2019-04-12

### Changed

* Do not start default resolver immediately for default connector.


## [0.1.3] - 2019-04-11

### Changed

* Start trust-dns default resolver on first use

## [0.1.2] - 2019-04-04

### Added

* Log error if dns system config could not be loaded.

### Changed

* Rename connect Connector to TcpConnector #10


## [0.1.1] - 2019-03-15

### Fixed

* Fix error handling for single address


## [0.1.0] - 2019-03-14

* Refactor resolver and connector services

* Rename crate
