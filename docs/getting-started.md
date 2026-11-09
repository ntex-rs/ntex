# Getting Started

Start with a small HTTP server, then add a route, a JSON endpoint, and shared
state. You need a Rust toolchain compatible with ntex 4; the current release
in this repository requires Rust 1.97 or later.

## Create a project

Create a binary crate:

```sh
cargo new hello-ntex
cd hello-ntex
```

Add ntex to the generated `Cargo.toml`:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio"] }
```

Use `tokio` as a starting choice, especially if your application will use
Tokio-based libraries. You still start the application with `#[ntex::main]`,
not `#[tokio::main]`, because ntex sets up its own runtime context too.

An explicit runtime feature is not required. Without one, ntex uses its
native runtime: Linux tries `io-uring` and falls back to polling, other Unix
systems use polling, and Windows uses IOCP. To choose another backend,
replace `tokio` with `compio`, `neon-polling` (native polling on Unix), or
`neon-uring` (native `io-uring` on Linux, without the polling fallback).
Choose one rather than combining runtime features. See
[Runtime](./4-runtime.md) for the runtime model.

The only default feature is `ws`, which enables WebSocket support. TLS
(`openssl` or `rustls`), compression (`compress`), and cookies (`cookie`) are
opt-in. You do not need them for this example. Enabling a feature makes its
APIs available; configuring HTTPS or installing compression middleware is a
separate step.

## Run your first server

Replace `src/main.rs` with this application:

```rust
use ntex::{SharedCfg, web};

async fn hello() -> &'static str {
    "Hello, ntex!"
}

fn config(cfg: &mut web::ServiceConfig) {
    cfg.route("/", web::get().to(hello));
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::HttpServer::new(async |_| web::App::new().configure(config))
        .bind("127.0.0.1:8080", SharedCfg::default())?
        .run()
        .await
}
```

`HttpServer` owns the listener and workers. Its asynchronous factory creates
an `App`, which maps `GET /` to the `hello` handler. Returning a string makes
it the response body. `SharedCfg` configures the listener's I/O and protocol
services; it is not application state.

Keep route registration in `config` from the start. Both the server and your
tests can call `App::new().configure(config)`, so tests exercise the same
routes you actually serve.

Run the application:

```sh
cargo run
```

In another terminal, try it:

```sh
curl http://127.0.0.1:8080/
```

The response body is `Hello, ntex!` (without a trailing newline). Stop the
server with Ctrl-C. If port 8080 is already in use, change the bind address
and the curl URL together. For listener and worker options, see
[Server](./5-server.md).

## Write your first test

Append this module to the same `src/main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ntex::{http::StatusCode, web::test};

    #[ntex::test]
    async fn hello_returns_a_greeting() {
        let app = test::init_service(web::App::new().configure(config)).await;
        let req = test::TestRequest::with_uri("/").to_request();
        let res = test::call_service(&app, req).await;

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(test::read_body(res).await.as_ref(), b"Hello, ntex!");
    }
}
```

Run it with `cargo test`. This test builds the application in process; it
opens no socket. Checking both the status and the body catches an unexpected
error response as well as an incorrect greeting. See [Testing](./testing.md)
for JSON requests, stateful applications, and real-server tests.

## Add a path parameter

Add a handler and replace `config` with this version:

```rust
async fn greet(name: web::types::Path<String>) -> String {
    format!("Hello, {}!", name.into_inner())
}

fn config(cfg: &mut web::ServiceConfig) {
    cfg.route("/", web::get().to(hello))
        .route("/hello/{name}", web::get().to(greet));
}
```

`GET /hello/Ada` now returns `Hello, Ada!`. The `Path` extractor turns a
captured route segment into a typed handler argument. For route patterns and
scopes, continue with [Routing and Handlers](./8.1-web-app.md). The
[Extractors and Responders](./8.4-web-app.md) chapter covers typed inputs and
response values.

## Accept and return JSON

Add serde alongside ntex in `Cargo.toml`:

```toml
serde = { version = "1", features = ["derive"] }
```

Add these types and handler, then replace `config` again:

```rust
#[derive(serde::Deserialize)]
struct GreetingRequest {
    name: String,
}

#[derive(serde::Serialize)]
struct GreetingResponse {
    message: String,
}

async fn create_greeting(
    body: web::types::Json<GreetingRequest>,
) -> web::HttpResponse {
    web::HttpResponse::Ok().json(&GreetingResponse {
        message: format!("Hello, {}!", body.name),
    })
}

fn config(cfg: &mut web::ServiceConfig) {
    cfg.route("/", web::get().to(hello))
        .route("/hello/{name}", web::get().to(greet))
        .route("/greetings", web::post().to(create_greeting));
}
```

Restart the server, then send a JSON body:

```sh
curl -X POST http://127.0.0.1:8080/greetings \
  -H 'Content-Type: application/json' \
  -d '{"name":"Ada"}'
```

The response body is `{"message":"Hello, Ada!"}`. `Json<T>` deserializes the
request with serde; `.json(&value)` serializes the response. See
[Extractors and Responders](./8.4-web-app.md) for limits and extractor
configuration, and [Error Handling](./8.5-web-app.md) for returning
application errors.

## Share application state

A counter needs to outlive one request. Wrap it in `AppState` so ntex can use
it as web application state, and use an `Arc` so every worker shares the same
atomic counter. Add the imports, type, and handler below, then replace
`config` and `main`. Keep the earlier handlers and JSON types.

```rust
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

type Counter = web::AppState<Arc<AtomicUsize>>;

async fn visits(state: &Counter, (): ()) -> String {
    let count = state.fetch_add(1, Ordering::Relaxed) + 1;
    count.to_string()
}

fn config(cfg: &mut web::ServiceConfig<Counter>) {
    cfg.route("/", web::get().to(hello))
        .route("/hello/{name}", web::get().to(greet))
        .route("/greetings", web::post().to(create_greeting))
        .route("/visits", web::get().to_with_state(visits));
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let state = Counter::new(Arc::new(AtomicUsize::new(0)));

    web::HttpServer::new(async move |_| {
        web::App::<Counter>::new()
            .configure(config)
            .build_with(state.clone())
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

`GET /visits` returns the next count. `to_with_state` passes application
state first, then request-local state (`()` here), then any extractors.
`build_with` supplies fixed application state rather than inheriting the
outer server's state. Cloning the `Arc` shares the counter instead of copying
its current value.

Update the test's application construction to supply a fresh counter, while
keeping the same `config` function and assertions:

```rust
let state = Counter::new(Arc::new(AtomicUsize::new(0)));
let app = test::init_service(
    web::App::<Counter>::new()
        .configure(config)
        .build_with(state),
)
.await;
```

For worker-local state, construct `Rc`-backed values inside worker
initialization instead. [Application State](./8.2-web-app.md) explains both
state access styles, and [Worker and Request State](./6-server-app.md) shows
where to initialize worker-local and process-wide resources.

## See request logs

ntex does not install access logging automatically. Add
`env_logger = "0.11"` to your dependencies, initialize it once in `main`
before creating the server, and add `Logger` before registering routes. For
the stateful application above, replace `main` with:

```rust
#[ntex::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    let state = Counter::new(Arc::new(AtomicUsize::new(0)));

    web::HttpServer::new(async move |_| {
        web::App::<Counter>::new()
            .middleware(web::middleware::Logger::default())
            .configure(config)
            .build_with(state.clone())
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

Run `RUST_LOG=ntex=info cargo run`, then make a request with curl. `Logger`
emits request and response information through the `log` crate at `info`
level; `env_logger` displays it according to `RUST_LOG`. The default format
includes the request line, status, response size, and elapsed time. The log
entry is written when the response body is dropped, normally after it has
been sent. See [Filters and Middleware](./8.3-web-app.md) for adding your own
request-processing layers.

## How ntex fits together

The web APIs sit on a common [Service Model](./1-service.md). A service
asynchronously turns an input into a response or an error. Inputs can be
connections, protocol messages, or HTTP requests; the same model connects
these layers. A [Pipeline](./2-pipeline.md) supplies state and coordinates a
service's readiness, calls, and shutdown.

The [Runtime](./4-runtime.md) drives asynchronous work and network events.
The [Server](./5-server.md) accepts connections and runs services on workers.
The web framework builds routing, extractors, and middleware on that
foundation, so you can begin with `HttpServer` and learn the lower layers
when you need them.

Below HTTP, the [I/O Abstraction Layer](./7-io.md) provides buffered
connections independent of the runtime's socket type. Filters can transform
the bytes, for example to implement TLS, without changing your protocol
handler. This becomes useful when you build a custom protocol or integrate
a transport rather than just add a web route.

## Where to go next

- [Routing and Handlers](./8.1-web-app.md)
- [Application State](./8.2-web-app.md)
- [Filters and Middleware](./8.3-web-app.md)
- [Extractors and Responders](./8.4-web-app.md)
- [Error Handling](./8.5-web-app.md)
- [Testing](./testing.md)
- [Cookbook](./cookbook.md)
