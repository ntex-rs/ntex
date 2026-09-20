# Web Application State

A [`web::App`] is a service factory parameterized by its application state
type. Normally, it inherits that state from the surrounding server or service
pipeline. Handlers, extractors, filters, middleware, and error responses can
all use the same state.

This page focuses on how state interacts with `web::App`. See
[Application state management](./6-server-app.md) for worker initialization
and process-wide versus worker-local data, and [Service state](./3-state.md)
for the underlying service-state model.

## The `web::State` Trait

Every web application state type must implement [`web::State`]:

```rust,ignore
pub trait State: 'static {
    type Error;
}
```

The trait has no methods. Its associated `Error` type identifies the
application's error domain and is used by [`WebError`] and
[`WebResponseError`] throughout handlers, filters, middleware, and fallback
services. Error handling is described in sepecrate document.

Use [`DefaultError`] when the application does not need a custom error domain:

```rust
use ntex::web;

#[derive(Clone)]
struct ApplicationState {
    greeting: String,
}

impl web::State for ApplicationState {
    type Error = web::DefaultError;
}
```

The state itself only needs to be `'static` to implement `web::State`.
Additional APIs may require `Clone`; in particular, the standard state
extractor and `build_with()` both clone the state.

## Inheriting Worker State

[`web::server_with_config()`] initializes one state value for each worker and
passes a reference to that state to the worker's application factory. The
`App` returned by the factory uses the same state type:

```rust,no_run
use std::io;

use ntex::{web, SharedCfg};

/// This is worker-level config which is available to web::App
#[derive(Clone)]
struct ApplicationState {
    greeting: String,
}

impl web::State for ApplicationState {
    type Error = web::DefaultError;
}

/// This is process-level config
struct ApplicationConfig;

impl ntex::server::ServerAppConfig for ApplicationConfig {
    type State = ApplicationState;

    async fn create(&self) -> io::Result<Self::State> {
        Ok(ApplicationState {
            greeting: "Hello".to_owned(),
        })
    }
}

async fn index(
    state: web::types::State<ApplicationState>,
) -> web::HttpResponse {
    web::HttpResponse::Ok().body(state.greeting.clone())
}

#[ntex::main]
async fn main() -> io::Result<()> {
    // We create process wide state, server will create worker level state
    // in each worker and will make it available for handler service.
    web::server_with_config(ApplicationConfig, async |_| {
        web::App::new().route("/", web::get().to(index))
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

Once the application is running, the
state is supplied through the service context for every request handled by
that worker.

## Accessing State in Handlers

[`web::types::State<St>`] is a request extractor that clones the current
application state and passes the clone to the handler. It implements `Deref`,
so state fields and methods can be accessed directly:

```rust
use ntex::web::{self, HttpResponse};

#[derive(Clone)]
struct ApplicationState {
    service_name: &'static str,
}

impl web::State for ApplicationState {
    type Error = web::DefaultError;
}

async fn status(
    state: web::types::State<ApplicationState>,
) -> HttpResponse {
    HttpResponse::Ok().body(state.service_name)
}
```

Because extraction clones the state for each request, state values commonly
contain cheap-to-clone handles such as `Rc`, `Arc`, client handles, or pool
handles. Worker-local state may use single-threaded types such as `Rc` and
`RefCell`; state shared between workers must use thread-safe synchronization.

Filters, middleware, and lower-level services do not need the extractor. They
receive a borrowed state through their service [`Ctx`].

## Supplying Fixed State with `.build_with()`

The configured application builder is represented by [`AppServices`] and can
be finalized with either `build()` or [`AppServices::build_with()`].

`build()` creates a service factory that uses the state supplied by its outer
service pipeline. `build_with(state)` instead stores a fixed application state
and adapts the application to any outer state type. The fixed state is used
when application services are created and whenever they process readiness,
requests, or shutdown.

This is useful when the HTTP or server pipeline has a different state type, or
no application state at all, but the web application needs its own state:

```rust
use ntex::web::{self, HttpResponse};

#[derive(Clone)]
struct ApplicationState {
    greeting: &'static str,
}

impl web::State for ApplicationState {
    type Error = web::DefaultError;
}

async fn index(state: web::types::State<ApplicationState>) -> HttpResponse {
    HttpResponse::Ok().body(state.greeting)
}

let app = web::App::<ApplicationState>::new()
    .route("/", web::get().to(index))
    .build_with::<()>(ApplicationState {
        greeting: "Hello",
    });
```

The `()` type argument is the outer pipeline state in this standalone
example. It is normally inferred when the application factory is passed to
[`HttpService`]. The application state must implement `Clone` because
`build_with()` clones it while creating application service instances.

`build_with()` substitutes the application state rather than adding another
state layer. Code outside the mapped application continues to use the outer
state, while web handlers and application middleware receive the fixed state.

## The `AppState<T>` Wrapper

[`AppState<T>`] is a convenience wrapper for state that uses `DefaultError`.
It implements `web::State`, `Clone`, `Default`, and `Deref<Target = T>` when
the wrapped type provides the required traits:

```rust
use ntex::web;

#[derive(Clone)]
struct Settings {
    service_name: &'static str,
}

let state = web::AppState::new(Settings {
    service_name: "users",
});

assert_eq!(state.service_name, "users");
assert_eq!(state.st().service_name, "users");
```

Define a dedicated state type and implement `web::State` directly when the
application needs a custom associated error type or additional state-specific
behavior.

[`AppServices`]: https://docs.rs/ntex/latest/ntex/web/struct.AppServices.html
[`AppServices::build_with()`]: https://docs.rs/ntex/latest/ntex/web/struct.AppServices.html#method.build_with
[`AppState<T>`]: https://docs.rs/ntex/latest/ntex/web/struct.AppState.html
[`Ctx`]: https://docs.rs/ntex-service/latest/ntex_service/struct.Ctx.html
[`DefaultError`]: https://docs.rs/ntex/latest/ntex/web/struct.DefaultError.html
[`HttpService`]: https://docs.rs/ntex/latest/ntex/http/struct.HttpService.html
[`WebError`]: https://docs.rs/ntex/latest/ntex/web/struct.WebError.html
[`WebResponseError`]: https://docs.rs/ntex/latest/ntex/web/trait.WebResponseError.html
[`web::App`]: https://docs.rs/ntex/latest/ntex/web/struct.App.html
[`web::State`]: https://docs.rs/ntex/latest/ntex/web/trait.State.html
[`web::server_with_config()`]: https://docs.rs/ntex/latest/ntex/web/fn.server_with_config.html
[`web::types::State<St>`]: https://docs.rs/ntex/latest/ntex/web/types/struct.State.html
