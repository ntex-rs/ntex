# Server

The ntex framework is single-threaded, but it provides a general-purpose
streaming server that can use multiple workers to distribute incoming
connections.

The ntex server accepts connections on one thread and distributes them across
worker threads. Each worker, in turn, passes the connection to a handler
service. The server can `bind` to or `listen` on any number of ports, each with
a different handler service.

## Starting a Server

Construct a server with
[`server::build()`](https://docs.rs/ntex/latest/ntex/server/fn.build.html).
Add a service by calling `bind()`, call `run()`, and await the returned server:

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
                    Ok::<_, std::io::Error>(Response::Ok().body("Hello world!"))
                })
            },
        )?
        .run()
        .await
}
```

The arguments to `bind()` are:

1. A service name used in logs and status reports.
2. An address that implements `ToSocketAddrs`.
3. A [`SharedCfg`](https://docs.rs/ntex-service/latest/ntex_service/cfg/struct.SharedCfg.html)
   value containing connection and protocol configuration.
4. An asynchronous service factory.

The factory receives the current worker's application state and creates the
service that handles accepted
[`Io`](https://docs.rs/ntex-io/latest/ntex_io/struct.Io.html) objects. In this
example, `HttpService` turns each connection into an HTTP service. The server
itself is protocol-independent, so the same builder can run other streaming
protocols.

`bind()` creates and owns the listening socket. Use `listen()` instead when an
application, supervisor, or socket-activation system has already created a
`std::net::TcpListener`. Unix platforms also provide `bind_uds()` and
`listen_uds()` for Unix domain sockets.

## Workers

ntex servers use multiple operating-system threads to take advantage of
multiple CPU cores. Each worker runs its own single-threaded runtime and owns
its own service instance. Each worker uses the runtime configured for `System`,
whether it is the default runner or a custom one.

By default, the server starts one worker for each available logical CPU. You
can set the number of workers explicitly:

```rust
let server = ntex::server::build()
    .workers(4)
    // Add services with `bind()` or `listen()`.
    # ;
```

The service factory passed to `bind()` is called once for each worker.
Therefore, services do not need to be `Send` or `Sync` after construction and
may use worker-local types such as `Rc` and `RefCell`. Data shared between
workers must use thread-safe synchronization primitives such as `Arc`, atomics,
or locks.

Each worker accepts up to 25,600 concurrent connections by default. You can
change this limit with `maxconn()`:

```rust
let server = ntex::server::build()
    .workers(4)
    .maxconn(10_000)
    # ;
```

When a worker reaches its limit, the server stops dispatching new connections
to that worker. If every worker reaches its limit, the listeners temporarily
stop accepting new connections until capacity becomes available.

## Server Configuration

[`ServerBuilder`](https://docs.rs/ntex-server/ntex_server/net/struct.ServerBuilder.html)
configures the accept loop and worker pool:

- `name()` sets the server and worker thread names.
- `workers()` sets the number of worker threads.
- `backlog()` sets the socket listen backlog and must be called before
  `bind()`.
- `maxconn()` sets the maximum number of concurrent connections per worker.
- `enable_affinity()` pins workers to CPU cores when possible.
- `stop_on_panic()` stops the entire server if a worker panics.
- `shutdown_timeout()` limits how long graceful worker shutdown can take. The
  default is 30 seconds.
- `status_handler()` receives accept-loop readiness updates.
- `disable_signals()` disables the server's built-in process signal handling.

A production configuration might look like this:

```rust
use ntex::time::Seconds;

let builder = ntex::server::build()
    .name("api")
    .workers(4)
    .backlog(1024)
    .maxconn(20_000)
    .shutdown_timeout(Seconds(15))
    .stop_on_panic();
```

## Controlling a Running Server

`run()` starts the accept loop and workers, then returns a cloneable
[`Server`](https://docs.rs/ntex-server/ntex_server/net/type.Server.html)
controller. Awaiting the controller waits for the server to stop:

```rust
let builder = ntex::server::build()
    // Add services with `bind()` or `listen()`.
    # ;

let server = builder.run();
server.await?;
```

A cloned controller can manage the server from another task:

```rust
let control = server.clone();

ntex::rt::spawn(async move {
    control.pause().await;
    control.resume().await;
    control.stop(true).await;
});

server.await?;
```

`pause()` stops accepting new connections without closing active connections.
`resume()` starts accepting connections again. `stop(true)` performs a graceful
shutdown, allowing workers to finish active work within the configured shutdown
timeout. `stop(false)` stops them immediately.

Signal handling is enabled by default. `SIGTERM` triggers a graceful shutdown,
while `SIGINT` triggers an immediate shutdown. On Unix, `SIGQUIT` also triggers
an immediate shutdown unless `graceful_shutdown()` is enabled. Applications
that install their own signal handlers should call `disable_signals()` and stop
the server through its controller.

The server cleanly separates three responsibilities:

```text
listeners -> accept loop -> worker-local services
```

The accept loop owns the listeners, the worker pool provides parallelism, and
the service abstraction defines how each connection is processed. This design
keeps protocol implementations independent of socket management and server
lifecycle concerns.

## Connection and Protocol Configuration

The server's `bind()` method requires a `SharedCfg` value. `SharedCfg` is a
container for the configuration used by a connection and its protocol services.

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
                .set_disconnect_timeout(Seconds(1))
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

When the server accepts a connection, it associates the `SharedCfg` value with
the resulting `Io` object. The I/O layer retrieves `IoConfig` and applies its
connection-level settings.

Protocol services and acceptors retrieve their own configuration types from the
same `SharedCfg` value. In this example, the HTTP service uses
[`HttpServiceConfig`](https://docs.rs/ntex/ntex/http/struct.HttpServiceConfig.html)
to configure its header limit and keep-alive timeout.

Other configuration types include:

- [`TlsConfig`](https://docs.rs/ntex-tls/ntex_tls/struct.TlsConfig.html)
  configures the TLS handshake timeout.
- [`ClientConfig`](https://docs.rs/ntex/ntex/client/struct.ClientConfig.html)
  configures HTTP client timeouts, connection pooling, redirects, headers, and
  response limits.
- [`WsClientConfig`](https://docs.rs/ntex/ntex/ws/struct.WsClientConfig.html)
  configures WebSocket client connections, protocols, headers, timeouts, and
  frame-size limits.
- [`WebAppConfig`](https://docs.rs/ntex/ntex/web/struct.WebAppConfig.html)
  provides web application settings such as the host name, local address,
  security state, and application state.
- [`ntex_h2::ServiceConfig`](https://docs.rs/ntex-h2/ntex_h2/struct.ServiceConfig.html)
  configures HTTP/2 flow control, frame and header limits, concurrent streams,
  and protocol timeouts.
- [`ntex_mqtt::MqttServiceConfig`](https://docs.rs/ntex-mqtt/ntex_mqtt/struct.MqttServiceConfig.html)
  configures MQTT limits, QoS behavior, packet sizes, and protocol timeouts.

## Test Server

ntex includes a test server helper for integration tests. `test_server()` starts
the provided service on an available local port and returns a
[`TestServer`](https://docs.rs/ntex-server/ntex_server/net/struct.TestServer.html)
controller. The server runs with one worker on a separate operating-system
thread and does not install signal handlers.

```rust
use ntex::http::{HttpService, Response};
use ntex::{client::Client, server};

#[ntex::test]
async fn test_server_response() {
    let server = server::test_server(async || {
        HttpService::new(async |_| {
            Ok::<_, std::io::Error>(Response::Ok().body("Hello world!"))
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

Call `addr()` to get the listening address, or use `connect()` to open a
configured client `Io` connection. The `server()` method provides access to the
underlying server controller. You can call `stop()` to stop the test server
early, but this is usually unnecessary because it stops automatically when the
last `TestServer` clone is dropped.

Use `TestServerBuilder` when the server and client need custom `SharedCfg`
values. For tests that need custom listeners or transports,
`build_test_server()` provides access to the underlying `ServerBuilder`.
