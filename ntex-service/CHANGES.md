# Changes

## [1.2.0-beta.3] - 2023-06-21

* Add custom ContainerCall future

* Allow to turn ContainerCall to static

## [1.2.0-beta.2] - 2023-06-19

* Remove Deref for Container<T>

## [1.2.0-beta.1] - 2023-06-19

* Rename Ctx to ServiceCtx

## [1.2.0-beta.0] - 2023-06-16

* Enforce service readiness during call

* Introduce service sharable readiness

* Remove boxed rc service

## [1.0.2] - 2023-04-14

* Remove Rc<S> where S: Service as it brakes readiness check validity

## [1.0.1] - 2023-01-24

* Add `FnShutdown` service to provide on_shutdown callback

## [1.0.0-beta.0] - 2022-12-28

* Rename Transform to Middleware

* Drop FnService's shutdown helper

* Simplify Service::poll_shutdown() method

* Add forward_poll_ready and forward_poll_shutdown macros

## [0.3.3] - 2022-07-08

* Revert cleanups

## [0.3.2] - 2022-07-07

* Add ?Sized to Rc service #125

* Make AndThenFactory::new() public

* Cleanups

## [0.3.1] - 2022-01-03

* Do not depend on ntex-util

## [0.3.0] - 2021-12-30

* Remove fn_transform

## [0.3.0-b.0] - 2021-12-24

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
