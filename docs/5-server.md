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
* `affinity()` sets the CPU placement of workers. Workers take one logical CPU
  of each physical core first, spread across NUMA nodes and L3 caches, then
  the SMT siblings. `Affinity::Cpu` pins a worker to one logical CPU,
  `Affinity::Core` to the logical CPUs of one physical core, and
  `Affinity::Soft` only sets the preferred CPU (the Windows ideal processor),
  so the scheduler can still move a worker away from a busy CPU. On other
  platforms `Soft` works like `Core`. The default is `Soft` on Windows and
  `Core` elsewhere, `Affinity::Disabled` leaves placement to the OS.
* `load_balancing()` selects how new connections are distributed between
  workers. The default `LoadBalancing::LeastLoaded` picks two random workers
  and sends the connection to the one with the lower runtime utilization
  (averaged over ~250 ms), or, when both are similarly busy, the one with
  fewer open and queued connections. Workers whose runtime is blocked by a
  task are skipped. It helps when connections are long-lived or their load is
  uneven. `LoadBalancing::RoundRobin` sends connections to the workers in turn.
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

## Server metrics

`Server::metrics()` returns a `ServerMetrics` snapshot. The call is
synchronous and cheap enough to run periodically. Counters and durations are
totals since the server started, so take two snapshots and use their
differences to get rates. Gauges (`active_connections`, `accept_backlog`, the
`*_active` and `*_buffered` values, cached pages) describe the moment of the
snapshot.

```rust,no_run
use ntex::server::Server;
use ntex::time::{Seconds, sleep};

async fn report(server: Server) {
    let mut prev = server.metrics();
    loop {
        sleep(Seconds(10)).await;
        let cur = server.metrics();

        let paused = cur.accept_paused_duration - prev.accept_paused_duration;
        println!("accept paused {paused:?}, backlog {}", cur.accept_backlog);

        for (w, p) in cur.workers.iter().zip(&prev.workers) {
            println!(
                "worker {:?}: {} new, {} active, not ready {:?}",
                w.id,
                w.items - p.items,
                w.active_connections,
                w.not_ready_duration - p.not_ready_duration,
            );
        }
        prev = cur;
    }
}
```

The worker list has one entry per worker slot, in the same order in every
snapshot. A restarted worker keeps the metrics of the worker it replaces.

### Accept loop

These fields are on `ServerMetrics`.

| Metric | Kind | Meaning |
|--------|------|---------|
| `accept_pauses` | counter | Number of times the accept loop stopped accepting, because no worker was ready or the server was paused. |
| `accept_paused_duration` | duration | Total time the accept loop was not accepting, including the current pause. |
| `accept_backlog` | gauge | Accepted connections waiting for a ready worker. |

ntex does not reject connections at the limit. While the accept loop is
paused, new connections wait in the operating system's listen backlog, so
these metrics are the main saturation signal:

* If `accept_paused_duration` grows steadily, all workers are at capacity.
  Add workers (`workers()`), raise `max_connections()` if workers have spare
  CPU and memory, or scale out.
* A non-zero `accept_backlog`, or short frequent pauses, means the server
  runs at its limit. Clients then see higher connect latency. If load comes
  in bursts, a larger `backlog()` keeps the operating system from dropping
  connection attempts.
* Pauses caused by `Server::pause()` are counted too. Account for them
  during maintenance windows.

### Listeners

`ServerMetrics::listeners` has one `ListenerMetrics` entry per bound socket.

| Metric | Kind | Meaning |
|--------|------|---------|
| `name`, `addr` | label | Service name and address passed to `bind()`. |
| `accepted` | counter | Connections accepted on the listener. |
| `connection_errors` | counter | Per-connection accept errors, for example a connection reset before it was accepted. |
| `resource_errors` | counter | Accept failures caused by resource exhaustion, for example running out of file descriptors. After each one the listener pauses briefly. |

The rate of `accepted` is the connection rate per service; compare it across
listeners to see where load comes from. A few `connection_errors` are normal
on the internet; a rising rate suggests clients that give up early or a
network problem. Treat any `resource_errors` as a capacity problem: raise
the file descriptor limit (`ulimit -n`) or lower `max_connections()`.

### Workers

`ServerMetrics::workers` has one `WorkerMetrics` entry per worker slot.

| Metric | Kind | Meaning |
|--------|------|---------|
| `id` | label | Worker id. |
| `items` | counter | Connections (or other items) the worker received. |
| `active_connections` | gauge | Open connections on the worker. Network servers only. |
| `not_ready` | counter | Number of times the worker's services stopped being ready, for example on reaching `max_connections()`. |
| `not_ready_duration` | duration | Total time the worker was not ready, including the current period. |
| `restarts` | counter | Number of times the worker was restarted after a failure. |
| `io`, `io_other` | | I/O statistics, see below. |
| `page_cache` | | Buffer page cache statistics of the worker thread, see below. |

* Compare `active_connections` and the rate of `items` across workers to
  check load balancing. With the default `LoadBalancing::LeastLoaded`, they
  should be close. A worker with much higher `not_ready_duration` than the
  others points to long-lived or expensive connections pinned to it.
* If `not_ready_duration` grows on all workers together with
  `accept_paused_duration`, the server is at capacity.
* `active_connections` at the `max_connections()` value with idle CPU
  means the limit is too low for the workload, for example with many idle
  keep-alive or WebSocket connections.
* `restarts` should stay at zero. Any increase means a worker failed;
  alert on it and check the logs.

### I/O metrics

I/O metrics are collected per I/O configuration (`IoConfig` in a
`SharedCfg`) on each worker thread. Every listener configuration is
registered automatically, so `WorkerMetrics::io` has one `IoConfigMetrics`
entry per listener configuration, identified by `id`, `tag` and `service` of
the `SharedCfg`. Other configurations, for example the configuration of an
HTTP client used by the application, get their own entry when created with
`IoConfig::set_metrics(true)`. Connections whose configuration has no
metrics of its own are counted together in `io_other`.

To get the totals of a service across the server, add up its entries from
all workers.

Each entry is an `IoMetricsSnapshot`:

| Metric | Kind | Meaning |
|--------|------|---------|
| `bytes_read` | counter | Bytes read from the transport. |
| `bytes_written` | counter | Bytes written to the transport. |
| `read_buffered` | gauge | Bytes held in read buffers, not yet consumed by the application. |
| `write_buffered` | gauge | Bytes in write buffers, or held by the transport and not yet written. |
| `read_backpressure` | counter | Number of times read backpressure was enabled: the read buffer reached `set_read_backpressure()` and reading stopped. |
| `read_backpressure_active` | gauge | Connections with read backpressure enabled now. |
| `read_backpressure_time` | duration | Total connection-time spent under read backpressure. |
| `write_backpressure` | counter | Number of times write backpressure was enabled: the write buffer reached `set_write_backpressure()`. |
| `write_backpressure_active` | gauge | Connections with write backpressure enabled now. |
| `write_backpressure_time` | duration | Total connection-time spent under write backpressure. |
| `eager_writes` | counter | Writes started in place, as soon as buffered output reached `set_write_buf_threshold()`. |
| `eager_writes_partial` | counter | Eager writes that could not write all output and left the rest to the write task. |
| `scheduled_writes` | counter | Writes left to the write task. With io-uring all writes use the write task. |
| `closed` | | Closed connections by reason, see below. |

Backpressure time is connection-time: ten connections under backpressure
for one second add ten seconds. Divide its rate by the number of active
connections to get the share of time a connection spends blocked.

* **Throughput.** Rates of `bytes_read` and `bytes_written` give the
  bandwidth per service and worker. Divided by the `items` rate they give
  the average traffic per connection.
* **Memory.** `read_buffered` plus `write_buffered` is the memory held in
  connection buffers. If it is high, lower `set_read_backpressure()` and
  `set_write_backpressure()`, or lower `max_connections()`.
* **Slow clients.** Growing `write_backpressure_time` means clients read
  slower than the application writes. A few slow clients are normal; a
  steady high `write_backpressure_active` holds memory and connection slots.
  Use `set_write_timeout()` to close clients that stop reading, and watch
  `closed.write_timeout`.
* **Slow application.** Growing `read_backpressure_time` means the
  application processes input slower than it arrives. Look at the service's
  processing time or worker CPU; a larger `set_read_backpressure()` only
  helps with short bursts.
* **Write path.** Most writes should be eager on fast networks. A high
  share of `eager_writes_partial` means the socket send buffer fills, which
  is normal for large responses and slow clients. If `scheduled_writes`
  dominate, output is written in many small pieces; raising
  `set_write_buf_threshold()` batches more output per system call,
  lowering it reduces latency.

### Close reasons

`IoMetricsSnapshot::closed` is a `CloseStats`. Each closed connection is
counted once in `total` and once under the first reason reported for it:

| Metric | Meaning |
|--------|---------|
| `total` | Closed connections. |
| `keepalive` | The connection was idle for longer than the keep-alive timeout. |
| `read_timeout` | Input did not arrive within the read timeout, for example `set_frame_read_rate()`. |
| `write_timeout` | Write backpressure stayed enabled for longer than `set_write_timeout()`. |
| `peer_gone` | The peer closed the connection. |
| `io` | The transport failed with an I/O error. |
| `decoder` | Incoming data could not be decoded, for example a malformed request. |
| `encoder` | Outgoing data could not be encoded. |
| `service` | The service failed or closed the connection. |
| `shutdown` | The server was shutting down. |
| `custom` | Application-defined reasons `Custom1` to `Custom8`, counted in `custom[0]` to `custom[7]`. |
| `other` | No reason was reported, for example the application closed the connection or the response had no keep-alive. |

* A high share of `keepalive` closes means connections sit idle. Shorten
  `set_keepalive_timeout()` to free connection slots sooner, or lengthen it
  if clients reconnect often (compare with the `accepted` rate).
* Rising `read_timeout` closes point to slow or idle clients, or to
  slow-read attacks; rising `write_timeout` closes to clients that stopped
  reading. Check the timeouts against legitimate slow clients before
  tightening them.
* `decoder` closes count protocol errors. A sudden increase usually means a
  broken client release, a scanner, or abuse.
* `io` and `peer_gone` closes reflect network quality and client behavior.
  An `io` spike together with `connection_errors` points to a network
  problem.
* `service` closes come from the application. Compare their rate with the
  application's error logs.
* `shutdown` closes count connections closed because the server or worker
  was stopping. Expect them during deploys and restarts; elsewhere they
  point to unexpected stops.

Applications can track their own reasons, for example authentication
failures, rate-limited clients or upgrades, by setting a reason before
closing the connection or failing the service:

```rust,ignore
use ntex::io::CloseReason;

// in a service that owns the connection's `Io`
io.set_close_reason(CloseReason::Custom1); // for example "auth failed"
io.close();
```

The first reported reason is kept, so a reason set by the application is
not replaced by one the dispatcher reports later, such as `Service`.

### Page cache

`WorkerMetrics::page_cache` holds the statistics of the worker thread's
buffer page cache. I/O buffers are allocated as pages of fixed sizes, and
released pages are kept in a per-thread cache for reuse. Use
`page_cache.get(size)` for one page size, `page_cache.iter()` for all sizes,
or `page_cache.total()` for the sum. Each entry is a `PageCacheStats`:

| Metric | Kind | Meaning |
|--------|------|---------|
| `size` | label | Page size, `BytePageSize::Unset` for totals. |
| `hits` | counter | Pages taken from the cache. |
| `misses` | counter | Pages allocated because the cache was empty. |
| `cached` | counter | Released pages returned to the cache. |
| `freed` | counter | Released pages freed because the cache was full. |
| `pages` | gauge | Pages in the cache now. |
| `bytes` | gauge | Memory held by the cached pages. |

* A high miss rate together with a high `freed` rate for the same size
  means the cache is too small for the workload: pages are freed and then
  allocated again. Raise its size for that page size with
  `ntex_bytes::set_page_cache_size()` on each worker thread, for example
  from the worker's service factory.
* If `bytes` stays high while traffic is low and misses are rare, the cache
  is larger than needed; lowering it frees memory.

After a worker restart these statistics may be slightly off, because the old
and new worker threads can update them at the same time.

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
