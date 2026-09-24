# Server

ntex uses a single-threaded runtime, but its server can run several workers to
make use of multiple CPU cores.

A dedicated accept loop accepts incoming connections and distributes them
between the workers. Each worker then passes its connections to a handler
service. A server can `bind` to or `listen` on several ports, with a different
handler service for each port.

## Starting a Server

Create a server with
[`server::build()`](https://docs.rs/ntex/latest/ntex/server/fn.build.html).
Add a service with `bind()`, call `run()`, and await the returned server:

```rust
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

`bind()` takes four arguments:

1. A service name used in logs and to match listeners with their services.
2. An address that implements `ToSocketAddrs`.
3. A [`SharedCfg`](https://docs.rs/ntex-service/latest/ntex_service/cfg/struct.SharedCfg.html)
   value containing connection and protocol settings.
4. An asynchronous service factory.

The factory is called for each worker and receives that worker's application
state. It creates the service that handles accepted
[`Io`](https://docs.rs/ntex-io/latest/ntex_io/struct.Io.html) objects.

In this example, `HttpService` handles each connection with the HTTP
protocol. The server itself does not depend on a particular protocol, so you
can use the same builder with other streaming protocols.

`bind()` creates and owns the listening socket. Use `listen()` when the socket
has already been created by the application, a supervisor, or a
socket-activation system. On Unix, `bind_uds()` and `listen_uds()` provide the
same functionality for Unix domain sockets.

## Workers

ntex servers use multiple operating-system threads to take advantage of
multiple CPU cores. Each worker runs its own single-threaded runtime and owns
its own service instance. Workers use the runtime configured for `System`,
whether it uses the default runner or a custom one.

By default, the server starts one worker for each available logical CPU. You
can choose a different number with `workers()`:

```rust
let builder = ntex::server::build().workers(4);
// Add services with `bind()` or `listen()`.
```

The service factory passed to `bind()` is called once for each worker. The
factory must be `Send` and `Clone`, but the service it creates never needs to
implement `Send` or `Sync`. This means that worker-local types such as `Rc`
and `RefCell` can be used inside a service.

Data shared between workers must still use thread-safe types such as `Arc`,
atomics, or locks.

Each worker can handle up to 25,600 concurrent connections by default. Use
`maxconn()` to change this limit:

```rust
let builder = ntex::server::build()
    .workers(4)
    .maxconn(10_000);
```

When a worker reaches its limit, the server stops sending new connections to
that worker. If all workers are at capacity, the listeners stop accepting
connections until space becomes available.

The limit is a process-wide setting shared by every server in the process.
`maxconn()` applies it immediately, and each worker reads it when it starts,
so call it before `run()`.

## Server Configuration

[`ServerBuilder`](https://docs.rs/ntex-server/ntex_server/net/struct.ServerBuilder.html)
provides several ways to configure the accept loop and worker pool:

- `name()` sets the server name, which is also used for the accept and worker
  thread names. It defaults to the system name.
- `workers()` sets the number of worker threads.
- `backlog()` sets the socket listen backlog. Call it before `bind()`.
- `maxconn()` sets the maximum number of concurrent connections per worker.
- `enable_affinity()` pins workers to CPU cores when possible.
- `stop_on_panic()` stops the entire server if a worker panics or its service
  cannot be created. Without it, a failed worker is restarted. The stop is
  graceful only if `graceful_shutdown()` is enabled.
- `graceful_shutdown_timeout()` sets the maximum time allowed for a graceful
  worker shutdown. The default is 30 seconds. This bounds the worker as a
  whole; each connection is bound separately by
  `IoConfig::set_shutdown_timeout`.
- `status_handler()` receives readiness updates from the accept loop.
- `disable_signals()` turns off the server's built-in signal handling.

A typical production configuration might look like this:

```rust
use ntex::time::Seconds;

let builder = ntex::server::build()
    .name("api")
    .workers(4)
    .backlog(1024)
    .maxconn(20_000)
    .graceful_shutdown_timeout(Seconds(15))
    .stop_on_panic();
```

## Controlling a Running Server

`run()` starts the accept loop and workers. It returns a cloneable
[`Server`](https://docs.rs/ntex-server/ntex_server/net/type.Server.html)
controller. Awaiting this controller waits for the server to stop:

```rust
let builder = ntex::server::build();
// Add services with `bind()` or `listen()`.

let server = builder.run();
server.await?;
```

You can clone the controller and manage the server from another task:

```rust
let control = server.clone();

ntex::rt::spawn(async move {
    control.pause().await;
    control.resume().await;
    control.stop(true).await;
});

server.await?;
```

`pause()` temporarily stops accepting new connections without closing active
ones. New connections wait in the kernel listen backlog. `resume()` starts
accepting connections again. The server also pauses itself while no worker is
available and resumes once one is ready.

Use `stop(true)` for a graceful shutdown. Workers are given time to finish
their active work, up to the configured shutdown timeout. Use `stop(false)` to
stop without waiting for the workers. Each worker still gets up to 3 seconds
to shut down its services.

Signal handling is enabled by default. On Unix:

- `SIGTERM` starts a graceful shutdown.
- `SIGINT` starts an immediate shutdown.
- `SIGQUIT` starts an immediate shutdown unless `graceful_shutdown()` is
  enabled.
- `SIGHUP` is ignored.

If panic handling is enabled for the runtime, `SIGSEGV`, `SIGABRT`, and
application panics also stop the server. Like `SIGQUIT`, these stops are
graceful only if `graceful_shutdown()` is enabled.

On Windows, only Ctrl-C is handled. It behaves like `SIGINT`.

Applications that install their own signal handlers should call
`disable_signals()` and use the server controller to stop the server.

The server separates three responsibilities:

```text
listeners -> accept loop -> worker-local services
```

The accept loop owns the listeners, the worker pool provides parallelism, and
the services decide how each connection is handled. This separation keeps
protocol implementations independent of socket management and the server
lifecycle.

## Connection and Protocol Configuration

The server's `bind()` method requires a `SharedCfg` value. This value acts as a
container for the configuration used by connections and protocol services.
Each component retrieves the configuration type it needs.

For example, `IoConfig` controls connection-level behavior such as timeouts,
read rates, and buffer sizes:

```rust
use ntex::http::{HttpService, HttpServiceConfig, Response};
use ntex::io::IoConfig;
use ntex::time::Seconds;
use ntex::util::BytePageSize;
use ntex::SharedCfg;

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let cfg = SharedCfg::new("HTTP")
        .add(
            IoConfig::new()
                .set_shutdown_timeout(Seconds(1))
                .set_write_page_size(BytePageSize::Size16),
        )
        .add(
            HttpServiceConfig::new()
                .set_max_headers(12)
                .set_keepalive_timeout(Seconds(10)),
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

When the server accepts a connection, it attaches the `SharedCfg` value to the
resulting `Io` object. The I/O layer reads the `IoConfig` value and applies its
connection-level settings.

Protocol services and acceptors read their own configuration types from the
same `SharedCfg` value. In this example, the HTTP service uses
[`HttpServiceConfig`](https://docs.rs/ntex/ntex/http/struct.HttpServiceConfig.html)
to set the maximum number of headers and the keep-alive timeout.

Other configuration types include:

- [`TlsConfig`](https://docs.rs/ntex-tls/ntex_tls/struct.TlsConfig.html)
  controls the TLS handshake timeout.
- [`ClientConfig`](https://docs.rs/ntex/ntex/client/struct.ClientConfig.html)
  controls HTTP client timeouts, connection pooling, redirects, headers, and
  response limits.
- [`WsClientConfig`](https://docs.rs/ntex/ntex/ws/struct.WsClientConfig.html)
  controls WebSocket client connections, protocols, headers, timeouts, and
  frame-size limits.
- [`WebAppConfig`](https://docs.rs/ntex/ntex/web/struct.WebAppConfig.html)
  contains web application settings such as the host name, local address,
  security state, and application state.
- [`ntex_h2::ServiceConfig`](https://docs.rs/ntex-h2/ntex_h2/struct.ServiceConfig.html)
  controls HTTP/2 flow control, frame and header limits, concurrent streams,
  and protocol timeouts.
- [`ntex_mqtt::MqttServiceConfig`](https://docs.rs/ntex-mqtt/ntex_mqtt/struct.MqttServiceConfig.html)
  controls MQTT limits, QoS behavior, packet sizes, and protocol timeouts.

## Test Server

ntex includes a small server helper for integration tests. `test_server()`
starts the provided service on an available local port and returns a
[`TestServer`](https://docs.rs/ntex-server/ntex_server/net/struct.TestServer.html)
controller.

The test server runs one worker on a separate operating-system thread and does
not install signal handlers:

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

Use `addr()` to get the listening address, or call `connect()` to open a client
`Io` connection using the test server's client configuration. The `server()`
method gives you access to the underlying server controller.

You can call `stop()` to stop the test server early, although this is usually
unnecessary. The server stops automatically when the last `TestServer` clone
is dropped.

Use `TestServerBuilder` if the server or client needs a custom `SharedCfg`
value. For tests that need custom listeners or transports,
`build_test_server()` gives you access to the underlying `ServerBuilder`.
