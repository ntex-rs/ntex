# Server

ntex servers use several single-threaded workers to make use of multiple CPU
cores. A dedicated accept thread owns the listening sockets and sends each
connection to an available worker:

```text
listeners -> accept thread -> worker-local service
```

The server does not know whether a connection speaks HTTP, MQTT, or a custom
protocol. It manages sockets and workers; the service created for each worker
decides how to handle the resulting `Io`.

## Starting a server

Create a server inside an active ntex `System`, register at least one listener,
and call `run()`:

```rust,no_run
use ntex::http::{HttpService, Response};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    ntex::server::build()
        .bind(
            "http",
            "127.0.0.1:8080",
            ntex::SharedCfg::default(),
            async |_| {
                HttpService::new(async |_| {
                    Ok::<_, std::io::Error>(
                        Response::Ok().body("Hello world!"),
                    )
                })
            },
        )?
        .run()
        .await
}
```

`server::build()` needs a current `System`, which `#[ntex::main]` creates in
this example. `run()` panics if no listener has been registered.

`bind()` receives:

1. a service name used in logs and to associate listeners with services;
2. an address that implements `ToSocketAddrs`;
3. a `SharedCfg` attached to accepted connections;
4. an asynchronous factory that builds the connection service for each
   worker.

`run()` returns a cloneable `Server` controller. Awaiting it waits for the
server to stop. The future always resolves to `Ok(())`; it is a completion
notification, not a health report for every connection or worker.

## Binding and existing listeners

`bind()` creates and owns the listening socket. If an address resolves to
several socket addresses, ntex tries all of them and registers every listener
that binds successfully. The call succeeds when at least one address binds.
Use a concrete `SocketAddr` when the application needs exactly one listener.

Use `listen()` when the application already owns a `TcpListener`, for example
with socket activation or custom socket options. The builder's `backlog()`
setting applies only to listeners created by later `bind()` and `configure()`
calls; it cannot change the backlog of an existing listener.

On Unix, `bind_uds()` and `listen_uds()` provide the same choices for Unix
domain sockets. `bind_uds()` removes an existing file at the socket path before
binding and removes the socket file again when the server stops.

A server may register several listeners and several protocols. Keep service
names clear and unique when using modular `configure()` callbacks, because
those callbacks attach worker services to listeners by name.

## Workers and service factories

By default, the server starts one worker for each available logical CPU. Set a
different number with `workers()`:

```rust,no_run
let builder = ntex::server::build()
    .workers(4);
```

Use at least one worker. With no workers, the server has nowhere to dispatch
connections and remains paused.

Each worker runs on its own arbiter thread and owns its own service instance.
The factory passed to `bind()` is called for every worker. It may be called
again if a worker restarts or a service is recreated after a readiness
failure, so factory setup should be safe to repeat.

The factory must be `Send`, `Clone`, and `'static`, but the service it creates
stays on one worker and does not need to implement `Send` or `Sync`. This is
why a worker-local service may use `Rc`, `Cell`, or `RefCell`.

Data shared across workers must be shared explicitly with thread-safe types
such as `Arc`, atomics, or locks. For richer process-wide and worker-local
state, use `build_with_config()` and `ServerAppConfig`; the next chapter
explains that model in detail.

Connections are distributed across workers that currently report ready. If a
worker reaches its connection limit, or one of its registered services is not
ready, that worker temporarily stops receiving new connections. If no worker
is available, the accept loop pauses until one becomes ready again.

The default limit is 25,600 concurrent connections per worker:

```rust,no_run
let builder = ntex::server::build()
    .workers(4)
    .max_connections(10_000);
```

This limit is process-wide and shared by every ntex server. Configure it before
starting servers; each worker takes the current value when its services are
created.

## Server configuration

The commonly useful `ServerBuilder` settings are:

* `name()` sets the accept and worker thread-name prefix. It defaults to the
  current system name.
* `workers()` sets the worker count.
* `backlog()` sets the listen backlog for listeners created afterward.
* `max_connections()` sets the process-wide per-worker connection limit.
* `enable_affinity()` pins workers to CPU cores when the platform exposes
  suitable core IDs.
* `graceful_shutdown_timeout()` bounds how long a graceful stop waits for
  workers. The default is 30 seconds.
* `stop_on_panic()` stops the server instead of restarting a worker that fails.
* `graceful_shutdown()` makes `stop_on_panic` worker failures, `SIGQUIT`,
  fatal signals, and application panics use graceful rather than immediate
  server shutdown.
* `disable_signals()` disables the server's built-in signal handling.
* `stop_runtime()` stops the complete ntex `System` after this server stops.

For example:

```rust,no_run
use ntex::time::Seconds;

let builder = ntex::server::build()
    .name("api")
    .workers(4)
    .backlog(1024)
    .max_connections(20_000)
    .graceful_shutdown_timeout(Seconds(15))
    .stop_on_panic();
```

`stop_runtime()` is useful when one server owns the complete process
lifecycle. Do not enable it when other independent work or servers must keep
using the same `System`.

`status_handler()` can connect listener readiness to a supervisor or metrics
system. It runs on the accept thread and reports when listeners pause or
resume. The status describes acceptance state, not whether every worker is
healthy, and the same status may be reported more than once. Keep the handler
quick and non-blocking.

## Controlling a running server

Clone the controller when another task needs to pause, resume, or stop the
server:

```rust,no_run
use ntex::server::Server;

async fn manage(server: Server) -> std::io::Result<()> {
    let control = server.clone();

    ntex::rt::spawn(async move {
        control.pause().await;
        // Perform maintenance or wait for an external readiness condition.
        control.resume().await;
        control.stop(true).await;
    });

    server.await
}
```

The server starts in a paused state and resumes once its first worker becomes
ready. `pause()` stops accepting new connections without closing existing
ones; new connection attempts normally wait in the operating system's listen
backlog. `resume()` enables the listeners again.

Use `stop(true)` for a graceful stop. The accept loop closes its listeners,
then the server asks workers currently available for dispatch to finish active
work and waits up to the configured graceful-shutdown timeout. A worker still
initializing or recreating its service is not part of that wait, so long-lived
factory setup should have its own cancellation or ownership boundary.

Use `stop(false)` when the controller should not wait for graceful worker
draining. Available worker services are still asked to shut down and get a
default three-second local timeout, but that cleanup may continue after the
server controller reports completion. An immediate stop is therefore not a
guarantee that every in-flight operation has finished.

Repeated stop requests join the stop already in progress. Await either
`stop(...)` or the original `Server` when later work must begin only after the
server has reached its stopped state.

## Signals and shutdown

Signal handling is enabled by default:

| Event | Default behavior |
|---|---|
| `SIGTERM` | graceful stop |
| `SIGINT` / Ctrl-C | immediate stop |
| `SIGQUIT` | immediate stop, or graceful with `graceful_shutdown()` |
| `SIGHUP` | ignored by the server |
| fatal signal or application panic | immediate stop, or graceful with `graceful_shutdown()` |

Fatal-signal notifications are installed with Unix signal handling.
Application-panic notifications additionally require runtime panic handling.
A fatal-signal shutdown is best effort because the operating system may
terminate the process before graceful cleanup finishes. On Windows, Ctrl-C is
delivered as the interrupt event.

Signal delivery belongs to the ntex `System`, not to one isolated server. If
several servers share a system, or the application installs its own signal
policy, call `disable_signals()` consistently and stop the appropriate server
controllers yourself.

The graceful server timeout and connection shutdown timeout solve different
problems. The server timeout bounds a worker as a whole. Each connection uses
its own `IoConfig::set_shutdown_timeout()` while flushing and closing. Leave
enough server-level time for those connection shutdowns to complete.

## Connection and protocol configuration

The `SharedCfg` passed to `bind()` or `listen()` is cloned onto each accepted
`Io`. The I/O layer and protocol services retrieve the configuration types
they understand from the same container:

```rust,no_run
use ntex::http::{HttpService, HttpServiceConfig, Response};
use ntex::io::IoConfig;
use ntex::time::Seconds;
use ntex::SharedCfg;

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let cfg = SharedCfg::new("HTTP")
        .add(
            IoConfig::new()
                .set_shutdown_timeout(Seconds(2)),
        )
        .add(
            HttpServiceConfig::new()
                .set_keepalive_timeout(Seconds(30)),
        );

    ntex::server::build()
        .bind("http", "127.0.0.1:8080", cfg, async |_| {
            HttpService::new(async |_| {
                Ok::<_, std::io::Error>(
                    Response::Ok().body("Hello world!"),
                )
            })
        })?
        .run()
        .await
}
```

`IoConfig` controls connection-level behavior such as shutdown, memory, and
read-rate limits. `HttpServiceConfig` controls HTTP behavior such as
keep-alive, request-head limits, and protocol timeouts. TLS and other protocols
add their own configuration types to the same `SharedCfg`.

The [I/O chapter](7-io.md) explains buffer limits, read rates, write
backpressure, keep-alive, and shutdown timing in detail.

## Test servers

`test_server()` starts one worker on a separate thread, binds an available
local port, and disables signal handling:

```rust
use ntex::http::{HttpService, Response};
use ntex::{client::Client, server};

#[ntex::test]
async fn test_server_response() {
    let server = server::test_server(async || {
        HttpService::new(async |_| {
            Ok::<_, std::io::Error>(
                Response::Ok().body("Hello world!"),
            )
        })
    });

    let url = format!("http://{}/", server.addr());
    let response = Client::new()
        .get(url.as_str())
        .send()
        .await
        .unwrap();

    assert!(response.status().is_success());
}
```

Use `addr()` for HTTP clients or `connect()` to open a client `Io` with the
test server's client configuration. `server()` returns the underlying server
controller.

`TestServerBuilder` can set separate server and client `SharedCfg` values.
`build_test_server()` exposes the full `ServerBuilder` for custom listeners;
when using it, set the address on the returned `TestServer` before calling
`connect()`.

Dropping the last `TestServer` clone stops its server and runtime. That drop
briefly blocks the current thread while the test thread shuts down, so drop it
outside timing-sensitive assertions.
