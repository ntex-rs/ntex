# State Management in ntex applications

Every ntex application has several types of state. Let’s discuss
each one individually.

## Process and Worker State

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

[Complete process/worker configuration example](https://github.com/ntex-rs/examples/tree/main/state-ch1)

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

The state discussed so far is long-lived. It stays alive for as long as the
worker, pipeline, or another object that owns it.

Sometimes, however, we need state that grows as a request or connection moves
through a service chain. Each service may add information to that state, and
the final service can extract both the accumulated state and the resulting
message before passing them to the next pipeline.

Consider a pipeline that accepts a new connection. The first service might read
the TLS `ClientHello` message and extract the Server Name Indication (SNI). The
next service could load configuration associated with that server name, validate
the connection, calculate resource usage, or apply throttling. Another service
could then perform the TLS handshake before passing the established connection
to an HTTP server.

The complete flow might look like this:

```text
accept connection
    → read SNI
    → load client information
    → validate and throttle
    → negotiate TLS
    → handle HTTP
```

For this kind of request-scoped state, ntex-service provides the `RequestState`
trait and the `State` type. Together, they allow services to pass a message and
its accumulated state through a service chain.

Unlike pipeline state, which remains available for the lifetime of the
pipeline, accumulated state belongs to the request or connection being
processed. Each item moving through the pipeline has its own state, and services
can add information to it as processing continues.

The current ntex protocol servers support `RequestState`, including
`ntex::http`, ntex-h2, ntex-mqtt, and ntex-amqp.
