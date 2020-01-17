# Changes

## [1.0.5] - 2020-01-16

### Fixed

* Fixed unsoundness in .and_then()/.then() service combinators

## [1.0.4] - 2020-01-15

### Fixed

* Revert 1.0.3 change

## [1.0.3] - 2020-01-15

### Fixed

* Fixed unsoundness in `AndThenService` impl

## [1.0.2] - 2020-01-08

### Added

* Add `into_service` helper function


## [1.0.1] - 2019-12-22

### Changed

* `map_config()` and `unit_config()` accepts `IntoServiceFactory` type


## [1.0.0] - 2019-12-11

### Added

* Add Clone impl for Apply service


## [1.0.0-alpha.4] - 2019-12-08

### Changed

* Renamed `service_fn` to `fn_service`

* Renamed `factory_fn` to `fn_factory`

* Renamed `factory_fn_cfg` to `fn_factory_with_config`


## [1.0.0-alpha.3] - 2019-12-06

### Changed

* Add missing Clone impls

* Restore `Transform::map_init_err()` combinator

* Restore `Service/Factory::apply_fn()` in form of `Pipeline/Factory::and_then_apply_fn()`

* Optimize service combinators and futures memory layout


## [1.0.0-alpha.2] - 2019-12-02

### Changed

* Use owned config value for service factory

* Renamed BoxedNewService/BoxedService to BoxServiceFactory/BoxService


## [1.0.0-alpha.1] - 2019-11-25

### Changed

* Migraded to `std::future`

* `NewService` renamed to `ServiceFactory`

* Added `pipeline` and `pipeline_factory` function


## [0.4.2] - 2019-08-27

### Fixed

* Check service readiness for `new_apply_cfg` combinator


## [0.4.1] - 2019-06-06

### Added

* Add `new_apply_cfg` function

## [0.4.0] - 2019-05-12

### Changed

* Use associated type for `NewService` config

* Change `apply_cfg` function

* Renamed helper functions

### Added

* Add `NewService::map_config` and `NewService::unit_config` combinators


## [0.3.6] - 2019-04-07

### Changed

* Poll boxed service call result immediately


## [0.3.5] - 2019-03-29

### Added

* Add `impl<S: Service> Service for Rc<RefCell<S>>`


## [0.3.4] - 2019-03-12

### Added

* Add `Transform::from_err()` combinator

* Add `apply_fn` helper

* Add `apply_fn_factory` helper

* Add `apply_transform` helper

* Add `apply_cfg` helper


## [0.3.3] - 2019-03-09

### Added

* Add `ApplyTransform` new service for transform and new service.

* Add `NewService::apply_cfg()` combinator, allows to use
  nested `NewService` with different config parameter.

### Changed

* Revert IntoFuture change


## [0.3.2] - 2019-03-04

### Changed

* Change `NewService::Future` and `Transform::Future` to the `IntoFuture` trait.

* Export `AndThenTransform` type


## [0.3.1] - 2019-03-04

### Changed

* Simplify Transform trait


## [0.3.0] - 2019-03-02

## Added

* Added boxed NewService and Service.

## Changed

* Added `Config` parameter to `NewService` trait.

* Added `Config` parameter to `NewTransform` trait.


## [0.2.2] - 2019-02-19

### Added

* Added `NewService` impl for `Rc<S> where S: NewService`

* Added `NewService` impl for `Arc<S> where S: NewService`


## [0.2.1] - 2019-02-03

### Changed

* Generalize `.apply` combinator with Transform trait


## [0.2.0] - 2019-02-01

### Changed

* Use associated type instead of generic for Service definition.

  * Before:

    ```rust
    impl Service<Request> for Client {
        type Response = Response;
        // ...
    }
    ```
  * After:

    ```rust
    impl Service for Client {
        type Request = Request;
        type Response = Response;
        // ...
    }
    ```


## [0.1.6] - 2019-01-24

### Changed

* Use `FnMut` instead of `Fn` for .apply() and .map() combinators and `FnService` type

* Change `.apply()` error semantic, new service's error is `From<Self::Error>`


## [0.1.5] - 2019-01-13

### Changed

* Make `Out::Error` convertable from `T::Error` for apply combinator


## [0.1.4] - 2019-01-11

### Changed

* Use `FnMut` instead of `Fn` for `FnService`


## [0.1.3] - 2018-12-12

### Changed

* Split service combinators to separate trait


## [0.1.2] - 2018-12-12

### Fixed

* Release future early for `.and_then()` and `.then()` combinators


## [0.1.1] - 2018-12-09

### Added

* Added Service impl for Box<S: Service>


## [0.1.0] - 2018-12-09

* Initial import
