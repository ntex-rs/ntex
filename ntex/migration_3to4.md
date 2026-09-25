# Migrating from ntex 3 to ntex 4

ntex 4 requires Rust 1.97 and upgrades the service stack to `ntex-service` 5.
Most migration work involves the new service state and lifecycle APIs.

## Server

Server service factories now create a `Service` directly. They no longer
create a `ServiceFactory` whose initialization configuration is `SharedCfg`.
The factory is called once per worker and receives a reference to that
worker's application state.

For a server without custom application state, continue to use
`ntex::server::build()`. The factory receives `&()`:

```rust,no_run
use std::io;
use ntex::http::{HttpService, Response};
use ntex::SharedCfg;

#[ntex::main]
async fn main() -> io::Result<()> {
    ntex::server::build()
        .bind(
            "http",
            "127.0.0.1:8080",
            SharedCfg::default(),
            async |_| {
                HttpService::new(async |_| {
                    Ok::<_, io::Error>(Response::Ok().body("Hello"))
                })
            },
        )?
        .run()
        .await
}
```

Use `ntex::server::build_with_config()` when each worker needs application
state. Its argument implements `ServerAppConfig` and creates the state for each
worker. An asynchronous closure can be used directly:

```rust,no_run
use std::io;
use ntex::SharedCfg;

#[derive(Clone)]
struct WorkerState;

#[ntex::main]
async fn main() -> io::Result<()> {
    ntex::server::build_with_config(
        async || Ok::<_, io::Error>(WorkerState),
    )
    .bind(
        "service",
        "127.0.0.1:8080",
        SharedCfg::default(),
        async |state: &WorkerState| {
            let state = state.clone();
            ntex::service::fn_service(move |_| {
                let _state = state.clone();
                async { Ok::<_, io::Error>(()) }
            })
        },
    )?
    .run()
    .await
}
```

`ntex-service` 5 also removes `Service::poll()`. The `Service` trait now uses
the asynchronous `ready()` and `shutdown()` lifecycle methods, while service
chains provide `readiness()` and `shutdown()` callbacks. `Pipeline` and
middleware APIs have been updated to bind and propagate service state.

### Runtime features

The deprecated `neon` feature has been removed from `ntex-net`. Remove it from
direct `ntex-net` dependencies. Select the `tokio`, `compio`, or `neon-uring`
feature when a specific runtime backend is required.

## HTTP services

`HttpService::openssl()` and `HttpService::rustls()` have been replaced by the
`http::openssl()` and `http::rustls()` service wrappers.

OpenSSL:

```rust,ignore
let service = ntex::http::openssl(
    acceptor,
    ntex::http::HttpService::new(handler),
);
```

rustls now accepts the ALPN protocol names separately:

```rust,ignore
let service = ntex::http::rustls(
    config,
    &["h2", "http/1.1"],
    ntex::http::HttpService::new(handler),
);
```

## HTTP client

The HTTP client builder, connector, and connection pool have been redesigned:

* `ClientBuilder` now owns the connector configuration; the separate client
  connector builder has been removed.
* Custom connector factories are replaced by connector `Service` values.
* `ClientBuilder::build()` is synchronous and takes a `SharedCfg`.
* Client settings are stored in `ClientConfig` inside `SharedCfg`.

The default connector is configured automatically:

```rust
use ntex::SharedCfg;
use ntex::client::{Client, ClientConfig};

fn create_client() -> Client {
    let cfg = SharedCfg::new("client").add(
        ClientConfig::new()
            .set_connection_limit(8)
            .set_response_payload_limit(256 * 1024),
    );

    Client::builder().build(cfg)
}
```

Use `ClientBuilder::connector()` or `ClientBuilder::secure_connector()` to
install a custom connector service.

## WebSocket client

WebSocket client settings have moved to `WsClientConfig`. Construct
`WsClient` directly with the URI and configuration; a separate builder is no
longer required:

```rust,ignore
use ntex::ws::{WsClient, WsClientConfig};

let client = WsClient::new(
    "ws://127.0.0.1:8080/ws",
    WsClientConfig::new().set_max_frame_size(128 * 1024),
);

let connection = client.connect().await?;
```

URI validation errors are now reported by `connect()` rather than by
`WsClient::new()`.

Custom connectors and TLS are still selected with `connector()`, `openssl()`,
or `rustls()` on `WsClient`.

## Web applications

Web application state and error handling have been redesigned.

### Application state in handlers

The application state type implements `web::State`. For common state that uses
the default web error type, `web::AppState<T>` provides a ready-made wrapper.
State is created once per worker with `web::server_with_config()` and is
passed to state-aware handlers by reference.

The `web::types::State<St>` extractor has been removed. Replace handlers that
use it with `Route::to_with_state()`. A state-aware handler receives:

1. A shared reference to the application state.
2. The request-local state.
3. Any request extractors.

```rust,no_run
use std::io;
use ntex::{web, SharedCfg};

#[derive(Clone)]
struct TestAppState {
    value: &'static str,
}

impl web::State for TestAppState {
    type Error = web::DefaultError;
}

async fn index(
    state: &TestAppState,
    _request_state: (),
) -> web::HttpResponse {
    web::HttpResponse::Ok().body(state.value)
}

#[ntex::main]
async fn main() -> io::Result<()> {
    web::server_with_config(
        async || {
            Ok::<_, io::Error>(TestAppState {
                value: "Hello",
            })
        },
        async |_| {
            web::App::<TestAppState>::new()
                .route("/", web::get().to_with_state(index))
        },
    )
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

The state-aware variants are available as `web::to_with_state()`,
`Route::to_with_state()`, `Resource::to_with_state()`, and
`ResourceServices::to_with_state()`. Continue to use `to()` when a handler only
needs request extractors.

The old `App::state()` model is replaced by service state for the primary
application state. Additional configuration values can be stored with
`WebAppConfig::set_state()` and retrieved with `HttpRequest::app_state()`.
Request-local state is available through `WebRequest::st()` and
`WebRequest::st_mut()`. Filters and middleware can change its type with
`WebRequest::map_state()`.

Custom implementations of the `Handler` trait must change `call()` from a
method returning `impl Future` to an `async fn`. Ordinary `async fn` handlers
do not need this change.

### Error handling

The `ErrorRenderer` API has been removed. The state's associated `Error` type
defines the application's error type, and errors are rendered through
`WebResponseError<St, Err>`. Error rendering receives the application state
instead of an `HttpRequest`. Service initialization errors now use
`ntex::error::Failure` and `IntoFailure`.

## Connection and protocol configuration

`SharedCfg` remains the container for connection and protocol configuration,
but it is separate from worker application state:

* Pass `SharedCfg` to server `bind()` or `listen()` methods.
* Pass `SharedCfg` to `ClientBuilder::build()`.
* Use `build_with_config()` or `web::server_with_config()` for per-worker
  application state.

See
[Connection and Protocol Configuration](docs/5-server.md#connection-and-protocol-configuration)
for a complete example.
