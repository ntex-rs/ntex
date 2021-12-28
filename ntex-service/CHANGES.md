# Changes

## [0.3.0] - 2021-12-24

* Service takes request type as a type parameter instead of an associated type

* ServiceFactory takes config type as a type parameter instead of an associated type

## [0.2.1] - 2021-09-17

* Simplify fn_transform

## [0.2.0] - 2021-09-15

* Refactor Transform trait

## [0.2.0-b.0] - 2021-08-26

* Simplify Transform trait

* Add PipelineFactory::apply() combinator

## [0.1.9] - 2021-06-03

* Add rc wrapped service, `RcService`

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
