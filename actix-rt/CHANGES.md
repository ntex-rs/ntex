# Changes

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
