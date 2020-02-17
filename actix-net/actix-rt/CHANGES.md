# Changes

## [1.0.0] - 2019-12-11

* Update dependencies

## [1.0.0-alpha.3] - 2019-12-07

### Fixed

* Fix compilation on non-unix platforms

### Changed

* Migrate to tokio 0.2


## [1.0.0-alpha.2] - 2019-12-02

Added

* Export `main` and `test` attribute macros

* Export `time` module (re-export of tokio-timer)

* Export `net` module (re-export of tokio-net)


## [1.0.0-alpha.1] - 2019-11-22

### Changed

* Migrate to std::future and tokio 0.2


## [0.2.6] - 2019-11-14

### Fixed

* Fix arbiter's thread panic message.

### Added

* Allow to join arbiter's thread. #60


## [0.2.5] - 2019-09-02

### Added

* Add arbiter specific storage


## [0.2.4] - 2019-07-17

### Changed

* Avoid a copy of the Future when initializing the Box. #29


## [0.2.3] - 2019-06-22

### Added

* Allow to start System using exsiting CurrentThread Handle #22


## [0.2.2] - 2019-03-28

### Changed

* Moved `blocking` module to `actix-threadpool` crate


## [0.2.1] - 2019-03-11

### Added

* Added `blocking` module

* Arbiter::exec_fn - execute fn on the arbiter's thread

* Arbiter::exec - execute fn on the arbiter's thread and wait result


## [0.2.0] - 2019-03-06

* `run` method returns `io::Result<()>`

* Removed `Handle`


## [0.1.0] - 2018-12-09

* Initial release
