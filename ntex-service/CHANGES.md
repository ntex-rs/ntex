# Changes

## [0.1.8] - 2021-04-11

* Move utils to ntex-util crate

## [0.1.7] - 2021-04-03

* drop futures-util dependency

* add custom Ready,Lazy,Either futures

## [0.1.6] - 2021-03-26

* Add .on_shutdown() callback to fn_service

## [0.1.5] - 2021-01-13

* Use pin-project-lite instead of pin-project

## [0.1.4] - 2020-09-24

* Add `fn_transform` fn, allows to use function as transform service

## [0.1.3] - 2020-04-15

* Upgrade pin-project

## [0.1.2] - 2020-04-27

* Check ready state for map_config_service

## [0.1.1] - 2020-04-22

* Add `map_config_service`, replacement for `apply_cfg`

## [0.1.0] - 2020-03-31

* Fork to ntex namespace

* Change Service trait to use `&self` instead of `&mut self`

* Add `fn_mut_service` for `FnMut` functions
