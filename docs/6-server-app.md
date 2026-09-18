## Application state management

Each ntex worker runs its own single-threaded runtime. To make use of multiple
CPU cores, the server starts several workers and distributes incoming
connections among them. Each worker then creates and runs its own application
instance.

Starting an application usually happens in two stages.

First, the server loads configuration that is shared across the entire process.
This normally happens on the main thread and may include reading environment
variables, parsing command-line arguments, loading configuration files, or
fetching settings from an external service.

Next, ntex uses that configuration to initialize each worker. The
`ServerAppConfig` trait controls this part of the process. You provide the
server with an object that implements `ServerAppConfig`, and the server calls
its `create()` method once inside each worker.

The configuration object must implement `Send + Sync` because the server shares
it across worker threads. The state returned by `create()` is different: it
belongs to one worker and stays on that worker's thread. As a result, worker
state can contain single-threaded types such as `Rc` and `RefCell`.

The `create()` method is asynchronous, so it can do more than simply construct
a struct. It can open database connections, create client instances, initialize
caches, or prepare any other resources the worker needs. If something goes
wrong, it can return an error rather than starting the worker with invalid
state.

```rust
struct AppBuilder {
    // Configuration shared across the process
}

struct AppState {
    // Resources owned by one worker
}

impl ServerAppConfig for AppBuilder {
    type State = AppState;

    // Called once inside each worker.
    async fn create(&self) -> io::Result<Self::State> {
        Ok(AppState {
            // Initialize this worker's resources.
        })
    }
}
```

### Creating the Worker Application

Once the worker state is ready, the server calls an application factory to
build the worker's application instance. The factory runs once per worker and
receives a reference to the state created by `ServerAppConfig::create()`.

```rust
#[ntex::main]
async fn main() -> std::io::Result<()> {
    // Load the process-wide configuration.
    let builder = AppBuilder {
        // ...
    };

    web::server_with_config(builder, async |_state: &AppState| {
        web::App::new().service(
            web::resource("/").to(async || {
                web::HttpResponse::Ok()
            }),
        )
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

Here, `AppBuilder` holds the process-wide configuration and implements
`ServerAppConfig`. For every worker, ntex calls `AppBuilder::create()` to
produce a new `AppState`. It then passes that state to the application factory,
which builds a separate `web::App` instance for the worker.

The factory can use the state while setting up the application and its
services. The same state is also available to the worker's connection-handler
pipelines throughout their lifecycle.

### Worker-Local and Process-Wide State

A worker creates its state once and reuses it for every connection it handles.
It does not create a fresh state for every connection.

This means that connections handled by the same worker see the same state.
Connections handled by different workers, however, use different state
instances. Updating local state in one worker does not automatically update the
state in another worker.

This separation is often useful. Because worker-local state never crosses
thread boundaries, it can use simple single-threaded data structures without
paying the cost of synchronization.

Sometimes state really does need to be shared across all workers. In that case,
it must be shared explicitly. One common approach is to wrap the shared value in
an `Arc` and clone it into each worker state. If the value is mutable, it will
also need a suitable synchronization mechanism, such as an atomic type,
`Mutex`, or `RwLock`.

The best choice depends on how the data is used. Prefer worker-local state when
workers do not need to coordinate. Use process-wide shared state only when
changes must be visible across workers.

[Complete process/worker configuration example](https://github.com/ntex-rs/examples/tree/main/state1)

## Service and Pipeline State

ntex-service 5 adds a state parameter to the `Service` trait. This gives every
service access to state while it is checking readiness, handling requests, or
shutting down.

A simplified version of the trait looks like this:

```rust
trait Service<St, Req> {
    type Res;
    type Error;

    async fn call(
        &self,
        req: Req,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error>;

    async fn ready(
        &self,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<(), Self::Error>;

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>);
}
```

Here, `St` is the type of state the service expects. Since it is part of the
`Service` definition, Rust can check that a service is always used with the
right kind of state.

The same state is available in all three lifecycle methods:

- `ready()` can use it while checking whether the service is ready for more work.
- `call()` can use it while handling a request.
- `shutdown()` can use it while the service is stopping.

You can think of a stateful service as an asynchronous function that receives
both the state and the request:

```rust
async fn my_service(
    state: &St,
    req: Req,
) -> Result<Res, Error> {
    // ...
}
```

The `Service` trait does not pass state as a separate argument. Instead, it
provides state through `Ctx`. The context also connects the service to its
pipeline, which manages readiness, request processing, and shutdown.

### Constructing a Pipeline

To call a service, you first place it in a `Pipeline`. A pipeline keeps the
service and its state together. It can contain a single service or a chain of
services, all of which can use the same state.

Here is a small example:

```rust
async fn my_service(
    state: &AppState,
    req: usize,
) -> Result<String, io::Error> {
    // Use the pipeline state to handle the request.
    todo!()
}

#[ntex::main]
async fn main() -> io::Result<()> {
    // Create a pipeline containing one state instance and one service.
    let svc = ntex::Pipeline::new(AppState::new(), my_service);

    // Both calls use the same AppState instance.
    let first = svc.call(1).await?;
    let second = svc.call(2).await?;

    println!("{first}:{second}");

    Ok(())
}
```

The `AppState` is created when the pipeline is built. It stays with the pipeline
and is reused for every call. Calling `svc.call()` does not create a new state
instance.

If the pipeline contains a chain of services, each compatible service can
access the same state. This makes it easy to share resources across a service
chain without adding a copy of those resources to every service.

### Worker state

Earlier, we saw that ntex creates a separate state instance for each server
worker. That state also becomes the state of the worker's connection-handler
pipeline.

When a worker starts, the server calls the application factory and passes it a
reference to the worker state. The factory builds a connection-handler service,
and the server places that service in a pipeline together with the worker
state.

```rust
/// Handles an I/O connection using worker-local state.
async fn handle_io(
    state: &AppState,
    io: ntex::io::Io,
) -> Result<(), io::Error> {
    // Process the connection using the worker state.
    Ok(())
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    // Load the process-wide configuration.
    let builder = AppBuilder {
        // ...
    };

    ntex::server::build_with_config(builder)
        .bind(
            "127.0.0.1:8080",
            SharedCfg::new("S"),
            async |_state: &AppState| {
                // Build the connection-handler service.
                ntex::service(handle_io)
            },
        )
        .run()
        .await
}
```

The factory is called once for each worker and receives that worker's
`AppState`. It can use the state to configure the service during construction.

The server places the returned service and the worker state in a pipeline. When
a connection is dispatched to `handle_io` service, the pipeline passes access to the
same `AppState` instance alongside the connection.

Each worker has its own state. Connections handled by the same worker share one
state instance, but connections handled by different workers do not
automatically share state.

## State Accumulation

The state we have discussed so far is long-lived. Worker state lives for as long
as the worker, and pipeline state lives for as long as the pipeline. In both
cases, the state is created once and reused across many calls.

Sometimes we need state with a shorter lifetime. In particular, we may want to
collect information while a request or connection moves through a service
chain. Each service can inspect what has already been collected, add more
information, and pass the updated state to the next service.

Imagine a pipeline that accepts an incoming connection. The first service might
record basic details such as the connection ID and peer address. The next
service could read the TLS `ClientHello` and extract the Server Name Indication
(SNI). Other services might load configuration for that server name, validate
the client, calculate resource usage, apply throttling, and finally perform the
TLS handshake before handing the connection to an HTTP server.

The pipeline might look like this:

```text
accept connection
    → collect connection information
    → read SNI
    → load client information
    → validate and throttle
    → negotiate TLS
    → handle HTTP
```

ntex-service provides the `RequestState` trait and the `State` type for this
kind of state. Together, they let us pass a message and its accumulated state
through a service chain.

`State` and types that implement `RequestState` are different from pipeline state.
Pipeline state is shared across calls, while accumulated state belongs to
a single request. Every connection moving through the pipeline carries
its own state.

The ntex protocol servers support `RequestState`, including `ntex::http`,
ntex-h2, ntex-mqtt, and ntex-amqp.

### Collecting Connection Information

Let's start with a service that receives a raw I/O connection and collects some
basic information about it:

```rust
use std::{io, net::SocketAddr, time::Instant};
use ntex::{io::Io, service::State};
use uuid::Uuid;

struct Connection {
    id: Uuid,
    created: Instant,
    peer_addr: SocketAddr,
}

async fn connect(_: &AppState, io: Io) -> io::Result<State<Connection, Io>> {
    let peer_addr = load_peer_addr(&io)?;

    Ok(State {
        req: io,
        state: Connection {
            id: Uuid::new_v4(),
            created: Instant::now(),
            peer_addr,
        },
    })
}
```

The next service can unpack these values, perform the TLS handshake, and add
more information:

```rust
use ntex::service::{RequestState, State};

struct ConnectionWithTls {
    id: Uuid,
    created: Instant,
    peer_addr: SocketAddr,
    server_name: String,
}

async fn tls(_: &AppState, msg: State<Connection, Io>) -> io::Result<State<ConnectionWithTls, TlsIo>> {
    let (connection, io) = msg.unpack();

    let server_name = load_server_name(&io).await?;
    let io = accept_tls(io, &server_name).await?;

    Ok(State {
        req: io,
        state: ConnectionWithTls {
            id: connection.id,
            created: connection.created,
            peer_addr: connection.peer_addr,
            server_name,
        },
    })
}
```

This service changes both parts of the input. It turns the raw `Io` connection
into a TLS-enabled `TlsIo`, and it extends the connection state with the server
name extracted during TLS negotiation.

The connection ID, creation time, and peer address are carried forward. By the
time the connection reaches the HTTP server, its state contains everything
collected by the earlier services.

### Passing Connection State to the HTTP Service

We can now connect these services into a single chain:

```rust
use ntex::{server, service, SharedCfg};
use ntex::http::{self, Request, Response};

/// Handles an HTTP request using information about its connection.
async fn handle_request(st: &ConnectionWithTls, _req: Request) -> io::Result<Response> {
    Ok(Response::Ok()
        .body(format!("server name: {}", st.server_name)),
    )
}

#[ntex::main]
async fn main() -> io::Result<()> {
    let builder = AppBuilder {
        // ...
    };

    server::build_with_config(builder)
        .bind(
            "HTTP",
            "127.0.0.1:8080",
            SharedCfg::new("S"),
            async |_: &AppState| {
                // Collect the initial connection information.
                service(connect)
                    // Perform the TLS handshake.
                    .and_then(tls)
                    // Pass the established connection to the HTTP service.
                    .and_then(http::HttpService::new(handle_request))
            },
        )?
        .run()
        .await
}
```

The worker's `AppState` is still available to `connect` and `tls`. It contains
worker-level resources that can be reused across many connections. At the same
time, each connection carries its own state.

[Complete state accumulation example](https://github.com/ntex-rs/examples/tree/main/state5)

## Implementation Details

ntex supports several network protocols, and each one handles state a little
differently. The important distinction is between state that belongs to a
connection and state that belongs to a single request.

### `ntex::http`

`HttpService` expects a handler with roughly the following shape:

```rust
async fn handler(state: &St, req: http::Request) -> Result<http::Response, Err> {
    // ...
}
```

Here, `state` is the state of the HTTP connection. It is extracted from the
value passed to `HttpService` through the `RequestState` trait.

For example, an earlier service can return a `State<ConnectionWithTls, Io>`.
When `HttpService` receives that value, it separates the connection state
from the I/O stream. The I/O stream is used to run the HTTP protocol,
while `ConnectionWithTls` becomes the state provided to the HTTP request handler.

This is the mechanism used in the previous section to make connection-specific
information, such as the peer address and TLS server name, available while
handling HTTP requests.

### `ntex::web`

A `web::App` can act as the handler for `HttpService`. When it does, the state
extracted by `HttpService` becomes the application's state.

A web service looks roughly like this:

```rust
async fn handler(state: &St, req: web::WebRequest) -> Result<web::WebResponse, Err> {
    // ...
}
```

The `state` argument contains the long-lived application or connection state.
The request can also carry its own state through the generic
parameter of `WebRequest`:

```rust
web::WebRequest<RequestState>
```

This gives a web application two separate kinds of state:

- `St` is provided by the service pipeline and remains available across
  requests.
- `RequestState` belongs to one request and moves through the request-processing
  chain.

A filter or middleware can inspect a `WebRequest`, perform some work, and
return a new `WebRequest` with a different state type. This makes it possible
to build up request-specific information as the request moves through
the application.

For example, a request-processing chain might look like this:

```text
validate request
    → load and authenticate the user
    → apply throttling
    → call the handler
```

The request starts without any additional state:

```rust
web::WebRequest<()>
```

After the authentication service loads and verifies the user, it can return:

```rust
web::WebRequest<AuthenticatedUser>
```

The next service can then require `WebRequest<AuthenticatedUser>`. This means it
cannot be called until authentication has completed and the request contains an
authenticated user.

Another service could validate the request and return a different state type:

```rust
web::WebRequest<ValidatedRequest>
```

In this way, the request type shows how far the request has moved through the
processing chain. Each service declares the state it expects and the state it
produces, and Rust checks that the services are connected in the right order.

There are therefore two kinds of state in a web application:

- Pipeline state contains application or connection information and
  is reused across requests.
- `WebRequest<ReqState>` contains information collected for one request and
  disappears when that request is complete.

Keeping them separate lets the application reuse connection-level resources
while building up request-specific information as each request moves through
the service chain.
