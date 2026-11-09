# Worker and request state

A database client is usually worth reusing across requests. The current
user's identity usually belongs to the request instead. Choose where to keep
state based on which requests or connections need to share it. A worker can
reuse long-lived resources while each request carries its own information.

Each ntex worker runs its own single-threaded runtime and application instance.
To use multiple CPU cores, the server starts several workers and distributes
incoming connections among them.

Application startup usually has two stages. First, you load configuration for
the whole process, normally on the main thread. You might read environment
variables, parse command-line arguments, load configuration files, or fetch
settings from an external service.

Next, ntex uses that configuration to initialize each worker. You provide an
object that implements `ServerAppConfig`. The server calls its `create()`
method inside each worker whenever that worker starts. If a worker fails and
restarts, `create()` runs again to give it fresh state.

The configuration object must implement `Send + Sync` because the server
shares it across worker threads. The state returned by `create()` belongs to
one worker and stays on its thread. You can use single-threaded types such as
`Rc` and `RefCell` in that state.

Worker state must also implement `Clone`: the server clones it when building
listener services. Servers such as `web::server_with_config()` clone it again
for every connection. To make those clones share a worker's data, put that
data behind a shared handle, such as `Rc<RefCell<T>>` or `Rc<Cache>`. Each
handle then refers to the same value. Cloning a plain field does not by itself
share later changes with other clones.

The asynchronous `create()` method can prepare resources the worker needs.
You can open database connections, create clients, or initialize caches there.
If initialization fails, return an error rather than start the worker with
invalid state.

```rust
struct AppBuilder {
    // Configuration shared across the process
}

#[derive(Clone)]
struct AppState {
    // Resources owned by one worker, usually behind `Rc`
}

impl ServerAppConfig for AppBuilder {
    type State = AppState;

    // Called each time a worker starts.
    async fn create(&self) -> io::Result<Self::State> {
        Ok(AppState {
            // Initialize this worker's resources.
        })
    }
}
```

You do not always need a separate configuration type. `ServerAppConfig` also
supports async closures that return `io::Result<State>`:

```rust
let server = ntex::server::build_with_config(async || {
    Ok(AppState {
        // ...
    })
});
```

For servers that need no state, `NoConfig` provides unit state.

## Creating the Worker Application

Once a worker has its resources, it can build the application that uses them.
The server calls each registered application factory in each worker, after
`ServerAppConfig::create()` finishes, and passes a reference to the worker's
state.

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

Here, `AppBuilder` implements `ServerAppConfig`, so its `create()` method
produces a fresh `AppState` for each worker. The application factory receives
that state and builds the worker's `web::App` factory.

There are two kinds of factory at work here. The application factory runs
during worker setup; the resulting `App` creates a service for each connection,
with its own clone of the worker state. You can therefore use state both when
setting up the application and its services, and throughout the lifecycle of
the worker's connection-handler pipelines.

## Worker-Local and Process-Wide State

An `Rc` shares data within one worker, not across the whole server. For a
value that every worker must see, make process-wide sharing explicit: wrap
the value in an `Arc` and clone it into each worker's state. Mutable values
also need suitable synchronization, such as an atomic type, `Mutex`, or
`RwLock`.

[Complete process/worker configuration example](https://github.com/ntex-rs/examples/tree/main/state/process)

## Service and Pipeline State

A service may need state before it accepts a request or after it finishes one.
The state parameter on `Service` makes state available while the service
checks readiness, handles requests, and shuts down.

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

`St` names the state type the service expects. Because it is part of the
`Service` definition, Rust checks that you use the service with the right
kind of state.

The same state is available throughout the lifecycle: `ready()` uses it while
checking whether the service can accept more work, `call()` while handling a
request, and `shutdown()` while stopping. It is not limited to request
handling.

For the request-handling part, a stateful service works like an asynchronous
function that receives both the state and the request:

```rust
async fn my_service(
    state: &St,
    req: Req,
) -> Result<Res, Error> {
    // ...
}
```

The `Service` trait provides state through `Ctx`, rather than a separate
argument. The context also connects the service to its pipeline. That
pipeline manages readiness, request processing, and shutdown.

### Constructing a Pipeline

To call a service, place it in a `Pipeline` with its state. A pipeline can
hold one service or a chain of services that use the same state.

```rust
use std::io;

struct AppState {
    prefix: &'static str,
}

async fn my_service(
    state: &AppState,
    req: usize,
) -> Result<String, io::Error> {
    Ok(format!("{} {req}", state.prefix))
}

#[ntex::main]
async fn main() -> io::Result<()> {
    // Create a pipeline containing one state instance and one service.
    let state = AppState { prefix: "request" };
    let svc = ntex::Pipeline::new(state, my_service);

    // Both calls use the same AppState instance.
    let first = svc.call(1).await?;
    let second = svc.call(2).await?;

    println!("{first}:{second}");

    Ok(())
}
```

This example creates `AppState` when it builds the pipeline. Both calls reuse
that instance; `svc.call()` does not create new state.

In a chain, each compatible service can access the same state. You can share
resources across the chain without storing a copy in every service.

### Worker state

The server builds the same service-and-state pairing for incoming connections.
When a worker starts, it passes a reference to that worker's state to the
application factory. The factory builds a connection-handler service, which
the server places in a pipeline with the state. Each worker has its own
instance, so this is a worker-local pipeline rather than a process-wide one.

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
            "handler",
            "127.0.0.1:8080",
            SharedCfg::new("S"),
            async |_state: &AppState| {
                // Build the connection-handler service.
                ntex::service(handle_io)
            },
        )?
        .run()
        .await
}
```

## State Accumulation

A request or connection may need to collect information as it moves through a
service chain. Each service can inspect the information so far, add to it, and
pass the updated state to the next service.

Unlike worker and pipeline state, accumulated state travels with the item
being processed. Worker state lasts for the worker's lifetime, and pipeline
state for the pipeline's lifetime; both are created once and reused across
many calls. Accumulated state belongs to one request, or to one connection
when the chain processes connections.

For example, the first service handling an incoming connection might record
its ID and peer address. The next could read the TLS `ClientHello` and extract
the Server Name Indication (SNI). Later services could load configuration for
that server name, validate the client, calculate resource usage, and apply
throttling. Finally, a service performs the TLS handshake before passing the
connection to an HTTP server.

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

The `ntex-service` crate provides the `RequestState` trait for passing a
message and its accumulated state through a service chain. `State<St, Req>`
is the most common implementation: it keeps the state and message together
as they move from one service to the next.

You can also use a `(St, Req)` tuple, which is convenient for small services.
`Io` and `IoBoxed` implement `RequestState` with `()` state. A plain connection
can therefore go to any service that expects a `RequestState`.

The ntex protocol servers support `RequestState`, including `ntex::http`,
ntex-h2, ntex-mqtt, and ntex-amqp.

### Collecting Connection Information

Start by wrapping the raw I/O connection with the information you want later
services to use. This service records a connection ID, creation time, and
peer address:

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

The next service unpacks those values, performs the TLS handshake, and adds
the server name:

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

The TLS service changes both parts of the input. It replaces the raw `Io`
connection with a TLS-enabled `TlsIo` and extends the state with the server
name extracted during TLS negotiation. The connection ID, creation time, and
peer address carry forward too, so the HTTP server receives everything the
earlier services collected.

### Passing Connection State to the HTTP Service

Connect the services in a chain to pass the collected state to the HTTP
handler. In this example, the handler uses the server name added by the TLS
service:

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

[Complete state accumulation example](https://github.com/ntex-rs/examples/tree/main/state/state-accumulation)

## Implementation Details

The connection chain above collects information once, but a connection may
carry many requests. Keep that distinction in mind when choosing where to
store state. Each ntex protocol exposes connection and per-request state a
little differently.

### `ntex::http`

`HttpService` passes connection state to the request handler while it runs
the HTTP protocol on the I/O stream. It separates its input into those two
parts using `RequestState`.

### `ntex::web`

A web application can use both long-lived state and state collected for one
request. When a `web::App` serves as the handler for `HttpService`, the state
extracted by `HttpService` becomes the application's state.

A web service looks roughly like this:

```rust
async fn handler(state: &St, req: web::WebRequest) -> Result<web::WebResponse, Err> {
    // ...
}
```

The `state` argument holds the long-lived application or connection state.
The generic parameter of `WebRequest` holds state for one request:

```rust
web::WebRequest<RequestState>
```

That means `St` comes from the service pipeline and remains available across
requests, while `RequestState` belongs to one request and moves through its
processing chain. A filter or middleware can inspect a `WebRequest`, do some
work, and return a new `WebRequest` with a different state type. This lets you
collect request-specific information without putting it in long-lived state.

For example, a request-processing chain might look like this:

```text
validate request
    → load and authenticate the user
    → apply throttling
    → call the handler
```

The request starts without additional state:

```rust
web::WebRequest<()>
```

After loading and verifying the user, the authentication service can return:

```rust
web::WebRequest<AuthenticatedUser>
```

The next service can require `WebRequest<AuthenticatedUser>`. It cannot be
called until authentication has completed and the request contains an
authenticated user.
