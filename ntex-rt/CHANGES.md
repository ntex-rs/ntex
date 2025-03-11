# Changes

## [0.4.26] - 2025-03-10

* Add Arbiter::spawn_with()

## [0.4.25] - 2025-03-10

* Add "ntex-runtime" support

* Drop glommio support

* Drop async-std support

## [0.4.24] - 2025-01-03

* Relax runtime requirements

## [0.4.23] - 2024-12-10

* Remove Unpin requirements for Arbiter::spawn()

## [0.4.22] - 2024-12-01

* Depend on individual compio packages

## [0.4.21] - 2024-11-25

* Update to compio 0.13

## [0.4.20] - 2024-10-17

* Allow to skip runtime feature for clippy run

## [0.4.19] - 2024-10-11

* Force runtime feature selection

## [0.4.18] - 2024-09-24

* Update to glommio v0.9

## [0.4.17] - 2024-09-24

* Update to compio v0.12

## [0.4.16] - 2024-09-02

* Allow to override block_on function

* Add stack size configuration

## [0.4.15] - 2024-08-30

* No runtime compatibility

## [0.4.14] - 2024-08-29

* Add `compio` runtime support

## [0.4.13] - 2024-04-04

* Use tokio Handle if available

## [0.4.12] - 2024-03-25

* Relax Arbiter::exec() generic param

## [0.4.11] - 2023-11-22

* Replace async-oneshot with oneshot

## [0.4.10] - 2023-11-02

* Upgrade async-channel to 2.0

## [0.4.9] - 2023-04-11

* Chore upgrade glommio to 0.8

## [0.4.8] - 2023-04-06

* Add enter/exit fn for spawn_cbs #195

## [0.4.7] - 2023-01-03

* Add callback for task spawning

## [0.4.6] - 2022-09-20

* Add System::block_on() helper method

* Fix async-std cannot find function block_on on windows

## [0.4.5] - 2022-06-20

* Add spawn_blocking stub

## [0.4.4] - 2022-02-20

* Upgrade to glommio 0.7

## [0.4.3] - 2022-01-17

* Add glommio runtime support

## [0.4.2] - 2022-01-11

* Enable all features for tokio runtime

## [0.4.1] - 2022-01-03

* Refactor async runtimes support

## [0.4.0] - 2021-12-30

* 0.4 release

## [0.4.0-b.3] - 2021-12-28

* Add `async-std` support

## [0.4.0-b.1] - 2021-12-22

* Fix lifetimes for unix_connect/unix_connect_in

## [0.3.2] - 2021-12-10

* Set spawn fn to ntex-util

## [0.3.1] - 2021-08-28

* Re-export time as different module

## [0.3.0] - 2021-08-27

* Do not use/re-export tokio::time::Instant

## [0.2.2] - 2021-04-03

* precise futures crate dependency

## [0.2.1] - 2021-02-25

* Drop macros

## [0.2.0] - 2021-02-23

* Migrate to tokio 1.x

## [0.1.2] - 2021-01-25

* Replace actix-threadpool with tokio's task utils

## [0.1.1] - 2020-04-15

* Api cleanup

## [0.1.0] - 2020-03-31

* Remove support to spawn futures with stopped runtime

* Fork to ntex namespace

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
