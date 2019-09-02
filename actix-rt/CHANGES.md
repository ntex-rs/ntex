# Changes

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
