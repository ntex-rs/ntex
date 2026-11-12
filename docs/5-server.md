# Server

An ntex server spreads connections across single-threaded workers to use
multiple CPU cores. A dedicated accept thread owns the listening sockets and
sends each connection to an available worker:

```text
listeners -> accept thread -> worker-local service
```

On Linux and Android, `LoadBalancing::Reuseport` removes the accept thread:
each worker accepts connections on its own listener. See
[Per-worker listeners](#per-worker-listeners).

The server manages sockets and workers without knowing the connection's
protocol. Each worker's service decides how to handle the resulting `Io`,
whether it carries HTTP, MQTT, or a custom protocol.

For encrypted connections, put a TLS acceptor before the protocol service.
Neither the listener nor the protocol implementation needs backend-specific
TLS logic. See [Securing Connections with TLS](10-tls.md) for backend
configuration, HTTPS helpers, and connection-level client-certificate
authentication.

## Starting a server

To start a server, create it inside an active ntex `System`. Register at least
one listener, then call `run()`:

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

`server::build()` needs a current `System`. In this example, `#[ntex::main]`
creates one.

`bind()` takes four arguments:

1. A service name, used in logs and to associate listeners with services.
2. An address that implements `ToSocketAddrs`.
3. A `SharedCfg` attached to accepted connections.
4. An asynchronous factory that builds the connection service for each
   worker.

`run()` returns a cloneable `Server` controller. Await it to wait for the
server to stop. The future always resolves to `Ok(())`. It reports completion,
not the health of every connection or worker.

`run()` panics if you have not registered a listener.

## Binding and existing listeners

Use `bind()` to let the server create and own a listening socket. Use
`listen()` when you already own a `TcpListener`, for example with socket
activation or custom socket options.

If an address resolves to several socket addresses, ntex tries all of them.
It registers every listener that binds successfully. The call succeeds when
at least one address binds. Use a concrete `SocketAddr` when you need exactly
one listener.

The builder's `backlog()` setting applies only to listeners created by later
`bind()` and `configure()` calls. It cannot change an existing listener's
backlog.

On Unix, `bind_uds()` creates a Unix domain socket, while `listen_uds()` uses
an existing listener. Before binding, `bind_uds()` removes any existing file
at the socket path. It removes the socket file again when the server stops.

You can register several listeners and protocols on one server. Keep service
names clear and unique when using modular `configure()` callbacks. Those
callbacks attach worker services to listeners by name.

## Workers and service factories

By default, the server starts one worker for each available logical CPU. Set a
different number with `workers()`:

```rust,no_run
let builder = ntex::server::build()
    .workers(4);
```

The worker count must be at least one. `workers(0)` panics because a server
without workers could never become ready or dispatch connections.

Each worker runs on its own arbiter thread and owns its own service instance.
The factory you pass to `bind()` runs for every worker. Its setup must be safe
to repeat. It may run again after a worker restarts or a readiness failure
causes a service to be recreated.

The factory must be `Send`, `Clone`, and `'static`. The service it creates
stays on one worker, so it does not need to implement `Send` or `Sync`. You can
use worker-local types such as `Rc`, `Cell`, or `RefCell` in that service.

Share data across workers explicitly, using thread-safe types such as `Arc`,
atomics, or locks. For richer process-wide and worker-local state, use
`build_with_config()` with `ServerAppConfig`. [Worker and request
state](./6-server-app.md) explains that model in detail.

Only workers that report ready receive connections. A worker stops receiving
new connections if it reaches its connection limit or any registered service
is not ready. If no worker is available, the accept loop pauses until one
becomes ready again.

The default limit is 25,600 concurrent connections per worker for this server:

```rust,no_run
let builder = ntex::server::build()
    .workers(4)
    .max_connections(10_000);
```

Other servers in the process keep their own connection limits.

## Server configuration

Most servers only need to choose their capacity and shutdown policy. Set
these on `ServerBuilder` before calling `run()`. Alongside `workers()` and
`max_connections()`, use `backlog()` to allow more connection attempts to
wait at listeners created afterward. Give the server a `name()` to identify
its accept and worker threads in logs; otherwise it uses the current system
name.

For shutdown, `graceful_shutdown_timeout()` sets how long workers may take to
finish; the default is 30 seconds. A failed worker normally restarts.
`stop_on_panic()` stops the server instead, and `graceful_shutdown()` makes
those failures, `SIGQUIT`, fatal signals, and application panics stop it
gracefully rather than immediately. Use `disable_signals()` if your
application will handle signals itself. The
[shutdown section](#signals-and-shutdown) explains how these policies interact.

For example, this builder gives each worker a connection limit and allows
15 seconds for a graceful stop:

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

Call `stop_runtime()` to stop the complete ntex `System` after this server
stops. It is disabled by default; enable it when one server owns the process
lifecycle. Leave it disabled if other work or independent servers must keep
using the same `System`.

The remaining placement settings, `affinity()` and `load_balancing()`, control
where workers run and which connections they receive. The next section
explains their defaults and when to change them.

Read `Server::status()` to check whether the server can accept connections.
Use `Server::status_changed()` to wait for the next change, for example in a
supervisor or health check. The states are:

* `Starting` until a worker becomes available, then `Ready`.
* `Unavailable` while no worker is available.
* `Paused` after `Server::pause()`, until you call `Server::resume()`.
* `Stopping`, followed by `Stopped`, when the server stops.

Listeners accept connections only while the server is `Ready`. This status
describes acceptance, not the health of every worker. Changes in quick
succession may produce a single notification with the latest status.

## Worker placement and load balancing

Worker placement and load balancing keep connection processing close to the
CPU receiving its packets. With the defaults, that CPU, or at least its
physical core, also handles the connection.

These are two separate decisions: `affinity()` places each worker thread
on a CPU, while `load_balancing()` chooses the worker for each new
connection. Keeping them close keeps packet data, socket state, and worker
data in the same CPU caches and on the same NUMA node. The network stack
also avoids waking a thread on another CPU for every received packet.

```rust,no_run
use ntex::server::{Affinity, LoadBalancing};

let builder = ntex::server::build()
    .workers(8)
    .affinity(Affinity::Core)
    .load_balancing(LoadBalancing::IncomingCpu);
```

### Worker placement

Worker placement spreads workers across physical cores before sharing them.
At startup, ntex reads the CPU topology and orders the logical CPUs the
process may use:

1. On hybrid CPUs, faster cores come first. Performance cores precede
   efficiency cores.
2. Within each class, ntex takes one logical CPU from every physical core.
   It interleaves them across the L3 caches of a NUMA node, then across NUMA
   nodes. With four workers on a two-socket machine, each socket gets two
   workers. They use different L3 caches if the socket has several.
3. The remaining SMT siblings (hyper-threads) follow in the same order.

Worker `i` takes entry `i` from this list. The list starts over when there are
more workers than CPUs. Workers never share a physical core until every core
has a worker. A restarted worker keeps its predecessor's placement.

The placement is applied with one of the `Affinity` modes:

| Mode | Windows | Linux and Android |
|------|---------|-------------------|
| `Disabled` | not placed | not placed |
| `Cpu` | pinned to one logical CPU (`SetThreadGroupAffinity`) | pinned to one logical CPU (`sched_setaffinity`) |
| `Core` | pinned to the logical CPUs of one physical core | pinned to the logical CPUs of one physical core |
| `Soft` | preferred CPU only, the ideal processor (`SetThreadIdealProcessorEx`) | same as `Core` |

The default is `Soft` on Windows and `Core` on Linux and Android. With `Core`,
the scheduler can move a worker between its core's SMT siblings, which share
L1 and L2 caches. Other platforms have no topology support and ignore
affinity.

ntex applies placement on the worker thread before creating its runtime.
The runtime, its timers, and the worker's buffer page cache are then allocated
on the NUMA node of the worker's CPU. ntex logs each placement at `info` level:

```text
Set affinity to Soft cpu 2 (core 2 3) for worker "api:worker:1"
```

When you start workers directly, pass a logical CPU index to `Worker::start()`.
On Windows, numbering continues from processor group 0 into group 1.

#### Windows

On a large Windows machine, the worker count should not be limited to the
CPUs in a single processor group. Windows divides machines with more than
64 logical CPUs into groups, and a thread runs in one group at a time. ntex
spreads workers across those groups while respecting explicit process CPU
limits.

ntex reads topology through `GetLogicalProcessorInformationEx`. This includes
physical cores and their efficiency classes, L3 caches, and NUMA nodes across
all processor groups. Only Windows 10 APIs are used.

`std::thread::available_parallelism()` counts only the current group. ntex
counts all groups for the default worker count and spreads workers across
them. A `Soft` worker's ideal processor must belong to its thread's group.
If that processor is in another group, ntex also gives the worker affinity to
that whole group.

The allowed CPUs come from:

* An explicit process affinity mask, such as `start /affinity` or job object
  affinity limits. This also keeps workers in the process's processor group.
* The group affinity of a job object the process runs in.
* The process's default CPU sets, set through `SetProcessDefaultCpuSets` or
  container CPU limits.

The groups where the process happens to run do not restrict placement.
Without explicit limits, ntex uses all groups.

`Soft` is a hint. The scheduler uses the ideal processor when it is free and
another CPU in the group otherwise. A worker can then avoid a busy thread or
interrupt-heavy CPU. On dedicated machines, use `Core` or `Cpu` when strict
isolation matters more.

#### Linux and Android

On Linux and Android, placement follows the process's allowed CPUs. Linux
has no preferred-CPU hint, so `Soft` works like `Core`.

ntex reads topology from sysfs under `/sys/devices/system/cpu/cpuN/`:

* `topology/core_cpus_list`, or `thread_siblings_list`, identifies the
  physical core.
* `cache/index3/shared_cpu_list` identifies the L3 cache. If there is none,
  ntex uses the package.
* The `nodeN` link identifies the NUMA node.
* `cpu_capacity` identifies big.LITTLE cores.

The allowed CPUs come from `sched_getaffinity()`. ntex therefore respects
`taskset`, cgroup cpusets (`docker run --cpuset-cpus`), and systemd's
`CPUAffinity=`.

The default worker count follows `std::thread::available_parallelism()`, which
also honors a cgroup CPU quota (`docker run --cpus`). A quota does not restrict
which CPUs run the process. Workers still spread across all allowed CPUs.

### Connection load balancing

Load balancing chooses which ready worker receives each new connection.
The accept thread makes that choice and queues the connection to the worker.
With `LoadBalancing::Reuseport`, the kernel chooses instead, see
[Per-worker listeners](#per-worker-listeners). The worker turns the socket
into an ntex `Io` and registers it with its own runtime. All later I/O for
that connection runs on the same worker.

Items submitted with `Server::process()` follow the same dispatch rules on
the submitting thread. While no worker is available, items wait in the
server's command loop until a worker is ready. That loop runs on the thread
that started the server.

Only ready workers receive connections. See
[Workers and service factories](#workers-and-service-factories) for readiness
rules. Among those workers, `LoadBalancing` decides:

* `RoundRobin` sends connections to workers in turn. It skips workers whose
  runtimes are blocked while another worker is available.
* `LeastLoaded` picks two random workers and chooses the one with lower
  runtime utilization. Utilization is the share of time the runtime was
  busy, averaged over about 250 ms and sampled at most every 10 ms. If the
  two differ by less than 10%, the one with fewer open and queued connections
  wins.

  Blocked workers are skipped while another worker is available. See
  [Stall detection](#stall-detection). On tokio, busy time is available only
  for current-thread runtimes. Otherwise, the connection count decides.
* `IncomingCpu`, the default, prefers a worker on the CPU that receives the
  connection's packets. Otherwise, it works like `LeastLoaded`.
* `Reuseport` lets each worker accept TCP connections on its own
  `SO_REUSEPORT` socket, without an accept thread. Linux and Android only,
  see [Per-worker listeners](#per-worker-listeners). Other connections and
  items work like `IncomingCpu`.

For each connection, `IncomingCpu` follows these steps:

1. Ask the operating system which CPU receives the connection's packets.
   This happens once, when the connection is accepted.
2. Find workers placed on that CPU. If there are none, find workers whose
   physical core contains it. This includes SMT siblings the process may not
   use, for example with `taskset`. The network stack may still process
   packets on those CPUs.
3. Choose an unblocked worker with the lowest utilization, then the fewest
   connections.
4. Compare it with the `LeastLoaded` choice. Use the local worker unless its
   utilization is more than 20% higher or its load exceeds the other worker's
   load by more than half plus 16 connections.

These limits prevent the preference from overloading workers. An adapter may
deliver traffic to only a few CPUs, or its hash may spread connections
unevenly. Some connections may also cost much more to process than others.
Under normal load, nearly every connection stays local.

Connections without an incoming CPU use the `LeastLoaded` choice. This
applies to:

* Loopback connections on Windows.
* Platforms other than Windows, Linux, and Android.
* `Affinity::Disabled`.
* Custom `ServerConfiguration` implementations that do not override
  `incoming_cpu()`.

A worker whose placement could not be applied is never preferred as local.
Connections that arrive while no worker is available wait in the server.
They follow the same balancing rules once a worker becomes available.

The incoming CPU is read only once. The connection stays on its worker even
if the operating system later moves its traffic to another CPU. This can
happen when RSS is rebalanced or irqbalance moves an interrupt.

The counters `ServerMetrics::incoming_cpu_hits` and `incoming_cpu_misses`
show how often connections stay local. See [Incoming CPU](#incoming-cpu).

#### Rescheduling

The server can move a queued connection if its worker cannot handle it.
A connection stays in the queue until the worker takes it. The server moves
it to another worker when the original worker:

* Fails, for example because its service panics. If no other worker is
  available, queued connections wait in the server for the restarted worker.
* Is not ready, for example while its service is recreated.
* Is blocked by a task. With two or more workers, the server checks for
  blocked workers every 50 ms. It moves their queued connections while
  another worker is not blocked. See [Stall detection](#stall-detection).

Only queued connections move. Once a worker takes a connection, it registers
that connection with its runtime and keeps it there. A blocked worker handles
it after becoming unblocked. A failed worker drops it.

Moved connections are balanced again without their incoming CPU. They are
not counted again in `incoming_cpu_hits` or `incoming_cpu_misses`.
`ServerMetrics::rescheduled` counts them instead. See
[Rescheduled connections](#rescheduled-connections).

#### Windows: receive-side scaling

Receive-side scaling (RSS) directs each connection's packets to a CPU.
The adapter hashes the connection's addresses and ports into an indirection
table that selects a receive queue. One CPU serves each queue and runs the
NDIS receive processing. ntex reads that CPU's processor group and number
with `WSAIoctl(SIO_QUERY_RSS_PROCESSOR_INFO)`.

With `Affinity::Soft`, the chosen worker's ideal processor is the RSS CPU.
The receive processing that completes its I/O usually runs on the same CPU
as the worker thread it wakes.

By default, RSS uses one logical CPU per physical core, starting at the
adapter's base processor. Worker placement also takes one logical CPU from
each core first. With one worker per core, the RSS CPUs have their own
workers.

Loopback connections (`127.0.0.1`, `::1`) do not pass through an adapter and
have no RSS CPU. Connections to the machine's own adapter addresses do have
one, including connections from WSL. You can use those addresses to test RSS
on a single machine. Adapters without RSS, or with RSS disabled, report none.

Use `Get-NetAdapterRss` to inspect the adapter's RSS settings. It shows
`BaseProcessorNumber`, `MaxProcessors`, and the processors used by the
indirection table. If many connections miss, align the RSS processors with
the worker CPUs, for example:

```powershell
Set-NetAdapterRss -Name "Ethernet" -BaseProcessorNumber 0 -MaxProcessors 8
```

#### Linux and Android: `SO_INCOMING_CPU`

On Linux 3.19 or later, `getsockopt(SOL_SOCKET, SO_INCOMING_CPU)` identifies
the CPU that processed the socket's last received packet. ntex reads this
value when accepting a connection. At that point, it identifies the CPU that
processed the last packet of the TCP handshake.

The adapter's receive-queue interrupt affinity sets that CPU through
`/proc/irq/N/smp_affinity_list`. Two mechanisms can change it:

* Receive Packet Steering moves packet processing to other CPUs. Its setting
  is `/sys/class/net/<dev>/queues/rx-N/rps_cpus`.
* Receive Flow Steering moves processing to the CPU where the consuming
  thread last ran.

Loopback packets are processed on the sending CPU, so loopback connections
report the client's CPU. Test servers on loopback therefore count hits and
misses too.

Use `ethtool -l <dev>` to check the number of receive queues. Change it with
`ethtool -L <dev> combined N`. Check `/proc/interrupts` to see which CPUs
handle the queues.

For the best locality, use one queue per worker and pin the queue interrupts
to the worker CPUs. irqbalance may move them again unless you stop it or tell
it to leave those interrupts alone.

#### Per-worker listeners

`LoadBalancing::Reuseport` removes the accept thread for TCP listeners on
Linux and Android. ntex replicates every bound TCP socket with
`SO_REUSEPORT`, one socket per worker, each with the `backlog()` length.
Each worker accepts on its own sockets on its own thread, so connections
never pass between threads.

```rust,no_run
use ntex::server::LoadBalancing;

let builder = ntex::server::build()
    .load_balancing(LoadBalancing::Reuseport);
```

The kernel chooses the socket for each new connection. ntex sets
`SO_INCOMING_CPU` on the sockets of each worker placed on a CPU. On Linux 6.1
or later, the kernel then sends the connection to the worker on the CPU that
processed its packets. Connections processed on a CPU without a worker are
spread by a hash of their addresses and ports, as are all connections with
`Affinity::Disabled`. Spread the receive queues, or RPS, over the worker CPUs
as described in the previous section.

A worker accepts only while it is ready, for example below
`max_connections()`. Otherwise, new connections wait in its socket's listen
backlog. The kernel has already chosen the socket, so ntex cannot move them:

* Utilization and blocked workers are not considered, and
  [Rescheduling](#rescheduling) does not apply. A blocked worker delays the
  connections queued on its socket until it unblocks.
* A failed worker's sockets stay open. Its replacement accepts the
  connections waiting there.
* `Server::pause()` stops every worker from accepting.
* When the server stops, workers close their sockets first. Connections
  still in a listen backlog are reset. Accepted connections finish within
  the graceful shutdown timeout as usual.

The accept thread still handles:

* Every listener on the tokio and compio runtimes. Reuseport requires the
  `neon-polling` or `neon-uring` runtime. ntex logs a warning and falls back
  to the accept thread.
* Every listener on platforms other than Linux and Android.
* Unix domain sockets.
* A TCP listener that cannot be replicated, for example when binding a copy
  fails. ntex logs `Cannot replicate <addr>, using an accept thread`.

Those connections, and items from `Server::process()`, are balanced like
`IncomingCpu`.

### Choosing the settings

Start with the defaults, then compare the workers' `active_connections`
and utilization. If connections stay uneven under `IncomingCpu`, try
`LoadBalancing::LeastLoaded`. An adapter with fewer queues than workers may
concentrate connections on only a few CPUs. Without a known incoming CPU,
`IncomingCpu` already falls back to `LeastLoaded`.

On machines shared with other busy processes, prefer `Affinity::Soft` on
Windows, or use `Affinity::Disabled`. The scheduler can then move workers
away from busy CPUs. If you have more workers than CPUs, several workers
share a CPU; `IncomingCpu` chooses the least loaded of those workers.

Use `LoadBalancing::RoundRobin` when you want connections assigned in turn,
with blocked workers skipped as described in
[Connection load balancing](#connection-load-balancing).

Consider `LoadBalancing::Reuseport` on Linux when the accept thread limits
the connection rate, for example with many short-lived connections. It
scales accepting with the workers and avoids a thread hop per connection.
It suits workers with similar connection costs that rarely block, because
the kernel never moves connections away from a busy worker. Check the
workers' `active_connections`, `not_ready_duration`, and `latency.stalls`
for imbalance. With uneven connections or frequent stalls, stay with
`IncomingCpu`.

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

Call `pause()` to stop accepting new connections without closing existing
ones. New connection attempts normally wait in the operating system's listen
backlog. Call `resume()` to enable the listeners again. At startup, the server
is paused until its first worker becomes ready, then resumes automatically.

Use `stop(true)` for a graceful stop. The accept loop closes its listeners.
The server then asks workers currently available for dispatch to finish
active work. It waits up to the configured graceful-shutdown timeout.

Use `stop(false)` when the controller should not wait for workers to finish
active work. Available worker services still receive a shutdown request and
a default three-second local timeout. That cleanup may continue after the
server controller reports completion. An immediate stop does not guarantee
that every in-flight operation has finished.

A graceful stop does not wait for workers still initializing or recreating
their services. Give long-lived factory setup its own cancellation or
ownership boundary.

Repeated stop requests join the stop already in progress. Await `stop(...)`
or the original `Server` if later work must wait until the server reaches
its stopped state.

## Server metrics

When clients wait longer to connect or requests slow down, server metrics
help you find where work is waiting. Start with the accept loop to see
whether new connections can get in, then compare workers and their I/O.
`Server::metrics()` returns a `ServerMetrics` snapshot. The call is
synchronous and cheap enough to run periodically.

Counters and durations are totals since the server started. Take two snapshots
and use their differences to get rates. Gauges describe the moment of the
snapshot. These include `active_connections`, `accept_backlog`, the `*_active`
and `*_buffered` values, and cached pages.

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

The worker list has one entry per worker slot. Its order stays the same
across snapshots. A restarted worker keeps its predecessor's metrics.

### Accept loop

Accept-loop metrics show when the server stops accepting connections and how
many accepted connections are waiting. These fields are on `ServerMetrics`.

| Metric | Kind | Meaning |
|--------|------|---------|
| `accept_pauses` | counter | Number of times the accept loop stopped accepting, because no worker was ready or the server was paused. |
| `accept_paused_duration` | duration | Total time the accept loop was not accepting, including the current pause. |
| `accept_backlog` | gauge | Accepted connections waiting for a ready worker. |

ntex does not reject connections at the limit. While the accept loop is
paused, new connection attempts wait in the operating system's listen
backlog. This is different from `accept_backlog`, which counts connections
the server has already accepted.

If `accept_paused_duration` grows steadily outside a deliberate pause,
check why workers are not ready. When all workers are at capacity, add
workers with `workers()` or scale out. You can also raise
`max_connections()` if workers have spare CPU and memory.

A non-zero `accept_backlog`, or short frequent pauses, indicates that
connections are waiting for workers. Clients then see higher connect
latency. For bursts of load, a larger `backlog()` keeps the operating system
from dropping connection attempts. Pauses caused by `Server::pause()` count
too, so account for maintenance windows when interpreting these numbers.

With `LoadBalancing::Reuseport`, connections accepted by workers never wait
in `accept_backlog`, and a worker that is not ready does not pause the
other workers. Only `Server::pause()` counts in `accept_pauses` and
`accept_paused_duration`. Use the workers' `not_ready_duration` to find
workers whose sockets hold waiting connections.

### Incoming CPU

These counters show how often `LoadBalancing::IncomingCpu` keeps a connection
on the CPU receiving its packets. Both fields are on `ServerMetrics`.

| Metric | Kind | Meaning |
|--------|------|---------|
| `incoming_cpu_hits` | counter | Connections sent to the worker on the CPU that receives their packets. |
| `incoming_cpu_misses` | counter | Connections with a known incoming CPU sent to another worker, because no worker is placed on that CPU or its worker was busier. |

Neither counter includes connections without a known incoming CPU. These
include Windows loopback connections and servers on platforms other than
Windows, Linux, and Android. Connections accepted by workers with
`LoadBalancing::Reuseport` are not counted either, because the kernel
chooses their worker.

Many misses mean packets reach CPUs without workers, or workers on the
receiving CPUs are busier than the others. The adapter might use CPUs outside
the process affinity or have fewer queues than the server has workers.

Compare the adapter's RSS settings with worker placement. Use
`Get-NetAdapterRss` on Windows. On Linux, use `ethtool -l` and
`/proc/interrupts`.

### Rescheduled connections

The `rescheduled` counter shows how often queued connections move to another
worker. This field is on `ServerMetrics`. See
[Connection load balancing](#rescheduling) for the dispatch rules.

| Metric | Kind | Meaning |
|--------|------|---------|
| `rescheduled` | counter | Connections taken from the queue of a failed, not ready or blocked worker and sent to another worker. |

An occasional increase follows a worker restart or stall. A steady increase
means workers are blocked often. Check `latency.stalls` for stalls and
`current_task` for the blocking code in [Runtime](#runtime). Connections
accepted by workers with `LoadBalancing::Reuseport` are never rescheduled.

### Listeners

Listener metrics show where connections arrive and why accepts fail.
`ServerMetrics::listeners` has one `ListenerMetrics` entry per bound socket.

| Metric | Kind | Meaning |
|--------|------|---------|
| `name`, `addr` | label | Service name and address passed to `bind()`. |
| `accepted` | counter | Connections accepted on the listener. |
| `connection_errors` | counter | Per-connection accept errors, for example a connection reset before it was accepted. |
| `resource_errors` | counter | Accept failures caused by resource exhaustion, for example running out of file descriptors. After each one the listener pauses briefly. |

Compare the rate of `accepted` across listeners to see where load comes from.
It gives the connection rate per service.

A few `connection_errors` are normal on the internet. A rising rate suggests
clients that give up early or a network problem. Treat any `resource_errors`
as a capacity problem. Raise the file descriptor limit with `ulimit -n` or
lower `max_connections()`.

### Workers

Compare worker metrics to check load balancing and find workers at capacity.
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
| `runtime` | | Runtime statistics of the worker thread, see below. |

Start by comparing `active_connections` and the rate of `items` across
workers. They should be close with the default
`LoadBalancing::IncomingCpu` or with `LoadBalancing::LeastLoaded`. With
`LoadBalancing::Reuseport`, they follow the kernel's choice and depend on
how the receive queues are spread over the worker CPUs. A worker
with much higher `not_ready_duration` than the others points to long-lived
or expensive connections pinned to it. If that duration grows on all
workers, together with `accept_paused_duration` when an accept thread is
used, the server is at capacity.

The connection limit can also stop workers that have CPU to spare. If
`active_connections` reaches `max_connections()` while CPU is idle, raise
the limit to suit the workload. Many idle keep-alive or WebSocket
connections can cause this.

Treat `restarts` separately from load: it should stay at zero. Any increase
means a worker failed. Alert on it and check the logs.

### Runtime

Runtime metrics help you find overloaded workers and tasks that delay other
work. `WorkerMetrics::runtime` contains a `RuntimeMetrics` for the worker
thread's runtime. A field is `None` when the runtime does not provide it.

Totals survive worker restarts. The `current_turn` and `current_task` fields
describe the current worker thread. So do the `latency` gauges: `last`,
`last_sample`, and `stalled`.

| Metric | Kind | Runtimes | Meaning |
|--------|------|----------|---------|
| `busy_duration` | duration | all | Time the runtime spent running tasks and handling I/O events. |
| `polls` | counter | neon, compio | Runtime turns that ran scheduled tasks. |
| `polls_again` | counter | neon, compio | Runtime turns that left scheduled tasks for the next turn. |
| `self_wakes` | counter | neon | Polls in which a task woke itself, for example by yielding. |
| `poll_histogram` | histogram | neon, compio | Duration of runtime turns. |
| `current_turn` | duration | neon, compio | How long the current runtime turn has been running, `None` while the runtime is idle. |
| `current_task` | location | neon | Spawn location of the task being polled now. |
| `latency` | | all | Scheduling latency and stalls, see below. `None` if sampling is disabled. |

The `latency` field contains a `LatencyMetrics`. A task on each worker sleeps
for `interval` and records how late it wakes up:

| Metric | Kind | Meaning |
|--------|------|---------|
| `interval` | setting | Sampling interval. |
| `last` | gauge | Lateness of the most recent sample. |
| `histogram` | histogram | Lateness of all samples. |
| `last_sample` | gauge | Time of the most recent sample. |
| `stalls` | counter | Times the worker recorded no sample for longer than `interval` plus the stall threshold. |
| `stall_duration` | duration | Total duration of finished stalls. |
| `stalled` | gauge | Whether the worker is stalled now. |

Utilization tells you the share of time the worker was busy; the next
section shows how to calculate it and explains runtime differences. A
worker close to 100% needs more CPU capacity, so consider adding workers
or cores. Uneven utilization points to expensive connections pinned to a
few workers.

Scheduling latency tells you how long ready work waits to run, adding to
request processing time. Watch high percentiles of the `histogram`
difference. Values of several milliseconds mean the worker is overloaded
or a task runs too long between `.await` points. Long turns in
`poll_histogram` help identify the latter: they delay every task woken
during the turn. Move such work to `spawn_blocking()` or split it.

If the ratio of `polls_again` to `polls` is close to 1, tasks are scheduled
faster than the worker runs them, leaving a backlog. A high `self_wakes`
rate instead points to tasks that busy-loop by yielding.

`stalls` should stay at zero. A stall means blocking code, such as a
synchronous call or a lock, held the worker. Alert on it.
[Worker stalls and busy loops](#worker-stalls-and-busy-loops) explains how
to find the task and set the sampling interval and stall threshold on the
system builder.

#### Worker utilization

To measure worker utilization, take two snapshots of `busy_duration`.
Divide its growth by the time between the snapshots. This running total
survives worker restarts:

```rust,no_run
use std::time::Instant;

use ntex::server::Server;
use ntex::time::{Seconds, sleep};

async fn utilization(server: Server) {
    let mut prev = server.metrics();
    let mut prev_time = Instant::now();
    loop {
        sleep(Seconds(10)).await;
        let cur = server.metrics();
        let now = Instant::now();
        let elapsed = (now - prev_time).as_secs_f64();

        for (w, p) in cur.workers.iter().zip(&prev.workers) {
            let (Some(busy), Some(prev_busy)) =
                (w.runtime.busy_duration, p.runtime.busy_duration)
            else {
                continue; // not reported by this runtime
            };
            let stalled = w.runtime.latency.as_ref().is_some_and(|l| l.stalled);
            let pct = if stalled {
                100.0
            } else {
                100.0 * busy.saturating_sub(prev_busy).as_secs_f64() / elapsed
            };
            println!("worker {:?}: {pct:.1}% busy", w.id);
        }
        prev = cur;
        prev_time = now;
    }
}
```

Use the same measurement window for all workers. A window of 5 to 10 seconds
smooths out short spikes. Shorter windows show bursts.

The runtimes report busy time differently:

* On neon and compio, `busy_duration` includes the turn running now.
  A worker held by a long task shows up as 100% busy right away.
* On tokio, `busy_duration` is reported only for current-thread runtimes.
  It excludes handling I/O events, so values are lower than on neon or
  compio for the same load.

  Tokio adds busy time only after a batch of tasks finishes. A stalled worker
  can therefore show low utilization. Treat a worker with `latency.stalled`
  set as fully busy, as the example does.

Compare utilization with the rate of `items` and with `active_connections`.
High utilization with few connections points to expensive requests. High
utilization on only one worker points to an imbalance, as described above.

### I/O metrics

I/O metrics show traffic, buffer use, and backpressure for each service and
worker. To get a service's totals across the server, add its entries from all
workers.

Each worker collects metrics per I/O configuration: an `IoConfig` in a
`SharedCfg`. ntex registers every listener configuration automatically.
`WorkerMetrics::io` has one `IoConfigMetrics` entry per listener
configuration. The `SharedCfg` fields `id`, `tag`, and `service` identify it.

Other configurations can have their own entries too, such as an HTTP client's
configuration. Enable them with `IoConfig::set_metrics(true)`. Connections
whose configuration has no separate metrics entry count together in
`io_other`.

I/O metrics are always collected. The default, `set_metrics(false)`, only
removes a configuration's separate entry. It does not affect registered
listener entries.

Each entry is an `IoMetricsSnapshot`:

| Metric | Kind | Meaning |
|--------|------|---------|
| `bytes_read` | counter | Bytes read from the transport. |
| `bytes_written` | counter | Bytes written to the transport. |
| `read_buffered` | gauge | Bytes held in read buffers, not yet consumed by the application. |
| `read_buffers_held` | gauge | Connections that keep a read buffer allocated for unconsumed input, such as the start of a frame that has not fully arrived. |
| `read_buffer_capacity` | gauge | Bytes allocated by those read buffers, a whole page for a pooled buffer. |
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

Backpressure time adds up across connections. Ten connections under
backpressure for one second add ten seconds. Divide its rate by the number
of active connections to get the share of time a connection spends blocked.

#### Traffic and buffer memory

Rates of `bytes_read` and `bytes_written` give bandwidth per service and
worker. Divide them by the `items` rate to get average traffic per
connection. For memory pressure, look at `read_buffer_capacity` and
`write_buffered` together. Lowering `set_read_backpressure()` and
`set_write_backpressure()`, or reducing `max_connections()`, can reduce
memory held in connection buffers.

A read buffer is released once its input is consumed, so busy connections
appear in `read_buffers_held` only briefly. A snapshot mostly shows idle
connections waiting for the rest of a frame. Each may keep a whole page
for a few bytes. If `read_buffer_capacity` far exceeds `read_buffered`,
these small leftovers are holding memory.

Many such connections point to slow or stalled clients. Use
`set_frame_read_rate()` to bound how long they can hold a buffer. A smaller
minimum page in `set_read_size()` lowers the cost of each. These metrics
exclude buffers held by pending io-uring reads and frames split off buffers
that keep their pages alive; [Io buffer attribution](#io-buffer-attribution)
counts them.

#### Read and write pressure

The direction of backpressure tells you which side cannot keep up. Growing
`write_backpressure_time` means clients read slower than the application
writes. A few slow clients are normal, but a persistently high
`write_backpressure_active` holds memory and connection slots. Use
`set_write_timeout()` to close clients that stop reading, and watch
`closed.write_timeout`.

Growing `read_backpressure_time` means the application processes input
slower than it arrives. Check the service's processing time or worker CPU.
A larger `set_read_backpressure()` only helps with short bursts.

On the write path, most writes should be eager on fast networks. A high
share of `eager_writes_partial` means the socket send buffer fills, which
is normal for large responses and slow clients. If `scheduled_writes`
dominate, output may be arriving in many small pieces. Raising
`set_write_buf_threshold()` batches more output per system call; lowering
it reduces latency. With io-uring, all writes are scheduled, so do not
interpret that counter as a sign of small writes.

### Close reasons

Close reasons help you distinguish timeouts and protocol errors from normal
connection closes. `IoMetricsSnapshot::closed` contains a `CloseStats`.
Each closed connection counts once in `total` and once under its first
reported reason:

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

A high share of `keepalive` closes means connections sit idle. Shorten
`set_keepalive_timeout()` to free connection slots sooner, or lengthen it
if clients reconnect often. Compare with the `accepted` rate to see the
tradeoff. Rising `read_timeout` closes point to slow or idle clients, or
slow-read attacks; rising `write_timeout` closes point to clients that
stopped reading. Check the timeouts against legitimate slow clients before
tightening them.

Protocol and transport failures need a different investigation. A sudden
increase in `decoder` closes usually means a broken client release, a
scanner, or abuse. The `io` and `peer_gone` reasons reflect network quality
and client behavior. An `io` spike together with `connection_errors` points
to a network problem.

For `service` closes, compare the rate with application error logs.
`shutdown` closes are expected during deploys and worker restarts, but at
other times they point to unexpected stops.

You can track application-specific reasons by setting a reason before closing
the connection or failing the service. Examples include authentication
failures, rate-limited clients, and upgrades:

```rust,ignore
use ntex::io::CloseReason;

// in a service that owns the connection's `Io`
io.set_close_reason(CloseReason::Custom1); // for example "auth failed"
io.close();
```

The first reported reason is kept. A later dispatcher reason, such as
`Service`, does not replace one your application already set.

### Page cache

The page cache reuses released I/O buffer pages to avoid repeated
allocations. Pages have fixed sizes, and each worker thread keeps its own
cache. Its statistics are in `WorkerMetrics::page_cache`.

Use `page_cache.get(size)` to read statistics for one page size. Use
`page_cache.iter()` for all sizes, or `page_cache.total()` for the sum.
Each entry is a `PageCacheStats`:

| Metric | Kind | Meaning |
|--------|------|---------|
| `size` | label | Page size, `BytePageSize::Unset` for totals. |
| `hits` | counter | Pages taken from this class's cache, including those used for requests for the next larger class. |
| `misses` | counter | Requests that found no cached page of the requested class. |
| `fallbacks` | counter | Misses served by a cached page one class smaller; also counted as a hit for that smaller class. |
| `cached` | counter | Released pages returned to the cache. |
| `freed` | counter | Released pages freed because the cache was full. |
| `pages` | gauge | Pages in the cache now. |
| `bytes` | gauge | Memory held by the cached pages. |

Subtract `fallbacks` from `misses` to count fresh page allocations. If that
rate and `freed` are both high for the same class, pages are being freed,
then allocated again. The cache is too small for the workload. Raise its
limit for that page size with `ntex_bytes::set_page_cache_size()` on each
worker thread, for example in its service factory.

If `bytes` stays high while traffic is low and misses are rare, the cache
is larger than needed. Lower its limit to free excess cached pages.
[Byte Buffers](./9-bytes.md#tuning-the-page-cache) explains these limits and
how they differ from memory held by live buffers.

Statistics may be slightly off after a worker restart. The old and new worker
threads can update them at the same time.

### Io buffer attribution

Use buffer attribution to measure input memory even after it leaves a
connection. You can measure how much memory a listener's request bodies hold
or find the connection holding the most input.

The I/O metrics count only read buffers a connection holds. A `Bytes` frame
split off a read buffer can keep the whole allocation alive in your
application. Buffers held by pending io-uring reads also keep allocations
alive. These do not appear in the read-buffer metrics.

Attribution charges each read buffer allocation to a `BytesTracker` for the
allocation's lifetime, wherever its data ends up. It requires the `tracker`
feature, which adds a pointer to every heap buffer's header:

```toml
ntex = { version = "4", features = ["tracker"] }
```

Configure attribution per `IoConfig` with `set_buf_tracker()`:

| `BufTracker` | Behavior |
|------------------|----------|
| `Disabled` | Read buffers are not attributed. This is the default. |
| `PerConnection` | Each connection charges its read buffers to its own tracker, created when first used. |
| `Shared(tracker)` | All connections of the configuration, on all workers, charge their read buffers to `tracker`. |

Use a shared tracker to get a total per listener or service. Create it before
the server starts and keep a clone so you can read it:

```rust,ignore
use ntex::io::{IoConfig, BufTracker};
use ntex::time::{Seconds, sleep};
use ntex::util::BytesTracker;
use ntex::SharedCfg;

let api_input = BytesTracker::new();
let cfg = SharedCfg::new("API").add(
    IoConfig::new().set_buf_tracker(BufTracker::Shared(api_input.clone())),
);

let server = ntex::server::build()
    .bind("api", "0.0.0.0:8080", cfg, async |_| { /* service */ })?
    .run();

ntex::rt::spawn(async move {
    loop {
        sleep(Seconds(10)).await;
        let snap = api_input.snapshot();
        println!(
            "api input: {} bytes in {} buffers",
            snap.live_bytes, snap.live_allocations
        );
    }
});
```

With `PerConnection`, use `IoRef::buf_tracker()` to get the connection's
tracker. In a handler, you can reach it through `HttpRequest::io()`. Filters
can use `IoContext::buf_tracker()`. These methods return `None` when
attribution is disabled.

Do not use the returned reference after calling `Io::set_config()`. Clone the
tracker if you need to keep it longer. A clone keeps reporting buffers that
outlive the connection without keeping the connection itself alive.

```rust,ignore
if let Some(tracker) = req.io().and_then(|io| io.buf_tracker()) {
    if tracker.snapshot().live_bytes > 64 * 1024 * 1024 {
        // the connection holds too much input, reject or close it
    }
}
```

You can also charge application buffers to the tracker. For example, you
might assemble a body from many frames, decompress a payload, or receive a
buffer from another source for the connection. That memory then appears
alongside the connection's read buffers.

With either `PerConnection` or `Shared`, pass the tracker to
`BytesMut::with_page_size_in(size, tracker)` when creating a buffer from the
page cache. For an existing buffer, call `BytesMut::set_tracker(tracker)`.
That moves any previous charge to the new tracker, including while `Bytes`
frames split off the allocation are still alive. Use `BytesMut::tracker()`
to find which tracker currently owns the charge.

```rust,ignore
use ntex::util::{BytePageSize, BytesMut};

let mut body = match req.io().and_then(|io| io.buf_tracker()) {
    Some(tracker) => BytesMut::with_page_size_in(BytePageSize::Size16, tracker),
    None => BytesMut::with_page_size(BytePageSize::Size16),
};
while let Some(chunk) = payload.next().await {
    body.extend_from_slice(&chunk?);
}
```

Application buffers follow the same rules as read buffers. The charge lasts
until the last handle is dropped. A growing buffer stays charged, while copies
are not charged.

Read `BytesTracker::snapshot()` to get a `BytesTrackerSnapshot`:

| Metric | Kind | Meaning |
|--------|------|---------|
| `live_bytes` | gauge | Memory held by the attributed allocations, including the allocation header. |
| `live_allocations` | gauge | Number of attributed allocations. |

Tracker values are separate from `ServerMetrics` and `IoMetricsSnapshot`.
Read the trackers you created and export their values alongside those metrics.
The two fields are read one after the other. A snapshot taken during buffer
allocation or release is therefore only approximately consistent.

Use `BytesTracker::ptr_eq()` to check whether two handles refer to the same
tracker.

#### What is charged

A tracker follows allocations, not just bytes waiting in a connection.
Attribution covers:

* Every read buffer the connection allocates or takes from the page cache.
  The charge covers the full allocation size, regardless of how much data
  it holds. A buffer that grows into a new allocation keeps the same tracker.
* Buffers of a pending read, including io-uring provided-buffer pages once
  they receive data. A buffer returned empty by a read goes back to the page
  cache, ending its charge.
* Plaintext read buffers of filters such as TLS.
* Frames split off a read buffer. The charge lasts until the last `Bytes` or
  `BytesMut` referring to the allocation is dropped. It ends on whichever
  thread drops that handle, even after the connection closes.

Attribution does not charge:

* Pages in the page cache. A cached page is charged again only to its next
  user.
* Write buffers.
* Copies of data. This includes small frames stored inline in a `Bytes`
  handle, up to 23 bytes on 64-bit targets, and copies made by
  `Bytes::trimdown()`. Data copied into application types is also uncharged
  unless you charge it explicitly as described above.

A shared view keeps the original allocation charged. Copies do not inherit
that charge: cloning a `BytesMut` copies its data, while splitting or
freezing a small result can copy it inline.

#### Configuration changes and cost

A configuration change through `Io::set_config()` can change attribution,
for example after a protocol upgrade:

* With `PerConnection` in both configurations, the connection keeps its
  tracker.
* Switching to `Shared(tracker)` moves the buffers the connection holds to
  `tracker`. Frames that already left the connection stay charged to the
  old tracker.
* Switching from `Shared` or `Disabled` to `PerConnection` creates a new
  tracker.
* Switching to `Disabled` stops charging new buffers. Existing buffers stay
  charged until they are released.

Without the `tracker` feature, attribution has no cost. Enabling it grows
every heap buffer's header from 16 to 32 bytes on 64-bit targets, or to
20 bytes on 32-bit targets, whether attributed or not. Allocating or growing
a buffer and releasing its last handle check for a tracker and update its
atomic counters when present. Sharing views needs no counter update, though
freezing the last mutable handle into inline storage releases its allocation
and ends its charge.

A `Shared` tracker also has a cache line updated by every worker thread,
which becomes contended under load. Prefer one tracker per listener or
service over one per server. Use `PerConnection` when you only need
per-connection limits.

#### Interpreting tracker values

Compare a shared tracker's `live_bytes` with the sum of
`read_buffer_capacity` for the same service. A much higher value points to
input outside the connection's current read buffer: request bodies queued
for later processing, cached frames, or pending io-uring reads. A small
part of a large buffer can hold the whole allocation; copy that part, for
example with `Bytes::trimdown()`, to release the buffer. If
`live_allocations` keeps growing while active connections stay stable,
check whether frames are ever dropped.

To estimate cost, divide `live_bytes` by `live_allocations` for the average
allocation size, or by `active_connections` for average input memory per
connection. A high average with little traffic points to the partial-input
case in [I/O metrics](#io-metrics).

With `PerConnection`, you can check a client's tracker before accepting
more input and close connections that exceed a limit. Unlike
`set_read_backpressure()`, this also counts input the connection already
handed to the application.

## Worker stalls and busy loops

Blocking a worker delays every connection it handles because they all share
one thread. A synchronous call, a long-held lock, or a long computation
between `.await` points can block that thread. The runtime detects blocked
workers and tasks that never wait. It logs them and reports them in worker
metrics.

### Stall detection

Stall detection identifies workers that stop making progress. Each worker
runs a task that sleeps for the latency sampling interval and records how
late it wakes up. A watchdog thread checks workers every interval.

A worker is stalled if it records no sample for longer than the interval
plus the stall threshold. The defaults are 100 ms for the interval and 1 s
for the threshold. A worker is therefore stalled after about 1.1 s without
progress.

When a stall starts, the watchdog logs an error:

```text
Arbiter my-server:worker:0(Id(3)) is stalled, no progress for 1.1s, running task spawned at src/main.rs:42:9
```

On neon, the message names the running task's spawn location. Tasks spawned
by ntex itself, such as connection dispatchers, report a location inside
ntex. A stall without a running task happens in the I/O driver.

When the worker makes progress again, a warning reports the stall duration:

```text
Arbiter my-server:worker:0(Id(3)) recovered after 2.4s stall
```

You can monitor the same information in `WorkerMetrics::runtime`.
`latency.stalls` and `latency.stall_duration` count finished stalls; alert
on any increase. During a stall, `latency.stalled` is set and
`current_turn` shows how long the worker has been blocked. On neon,
`current_task` identifies the blocking task's spawn location. Shorter
blocking that stays under the threshold instead appears as high values in
the `latency.histogram` and `poll_histogram` differences.

While another worker is available, new connections skip a worker if:

* Its runtime turn has lasted 100 ms or more.
* It has recorded no latency sample for 100 ms past its interval.
* It is stalled.

Queued connections move to other workers. See [Rescheduling](#rescheduling).
Connections the worker has already taken still wait for it.

Set the interval and threshold in milliseconds on the system builder.
`#[ntex::main]` has no options for them, so build the system directly:

```rust,no_run
use ntex::rt::{DefaultRuntime, System};

fn main() -> std::io::Result<()> {
    System::build()
        .name("my-server")
        .signals(true) // backtraces on Linux need signal handling
        .latency_interval(50) // sample every 50 ms
        .ping_threshold(500) // stalled after 550 ms without a sample
        .build(DefaultRuntime)
        .block_on(async {
            // build and run the server
            Ok(())
        })
}
```

A shorter interval detects stalls sooner and gives a finer latency
histogram. It also causes more timer wakeups on idle workers. Keep the
threshold above the longest blocking you accept to avoid logging expected
work.

Set `latency_interval(0)` to disable sampling and stall detection.
`WorkerMetrics::runtime.latency` is then `None`.

### Busy loop detection

A task can waste CPU and delay other tasks without stalling the worker.
A loop around `yield_now()` or `wake_by_ref()`, for example, wakes itself on
every poll instead of waiting for an event. The worker stays busy but is
not blocked.

The neon runtime counts these self-wakes. If a task wakes itself 10 000 times
in a row over at least one second, the runtime logs one warning for that
streak:

```text
Task spawned at src/main.rs:42:9 woke itself 10000 times in 1.2s without waiting, possible busy loop
```

`WorkerMetrics::runtime.self_wakes` holds the total count. A steadily rising
rate, high utilization (`busy_duration`), and a low connection rate point to
a busy loop. Replace it with code that waits for an event or timer.

The tokio and compio runtimes do not count self-wakes. To change the limits,
use `RuntimeBuilder::self_wake_threshold()` when building the runtime
directly. You cannot change them otherwise. See the runtime chapter.

### Backtrace capture

A stall backtrace helps you locate blocking code on the worker thread.
On Linux and Windows, the watchdog captures one backtrace when a stall starts:

* On Linux, it sends `SIGUSR2` to the worker thread. The signal handler
  records the stack. Enable ntex signal handling with `signals = true` in
  `#[ntex::main]`, or call `System::build().signals(true)`. Your application
  must not use `SIGUSR2` for anything else.
* On Windows, the watchdog briefly suspends the worker thread and walks
  its stack. This works on x86_64 and aarch64.
* Other platforms only log the stall.

By default, the watchdog resolves the backtrace and logs it as an error with
the task location. Use `System::set_latency_callback()` to replace that
logging, for example to send the backtrace to an error tracker.

The callback receives the unresolved backtrace and the blocking task's spawn
location. Keep it short because it is process-wide and runs on the watchdog
thread. The location is `None` if the task changed during capture. In that
case, the backtrace may belong to a different task.

```rust,no_run
use ntex::rt::System;

fn install() {
    System::set_latency_callback(|bt, task| {
        let bt = bt.resolve();
        log::error!("worker stalled, task spawned at {task:?}\n{bt}");
    });
}
```

Backtraces need symbols. Release builds without debug information may show
wrong or missing function names. Add `debug = "line-tables-only"` to the
release profile for readable backtraces with file and line numbers.

## Signals and shutdown

The server handles shutdown signals by default. You can use its built-in
policy or disable it and stop the server yourself:

| Event | Default behavior |
|---|---|
| `SIGTERM` | graceful stop |
| `SIGINT` / Ctrl-C | immediate stop |
| `SIGQUIT` | immediate stop, or graceful with `graceful_shutdown()` |
| `SIGHUP` | ignored by the server |
| fatal signal or application panic | immediate stop, or graceful with `graceful_shutdown()` |

Signal delivery belongs to the ntex `System`, not an isolated server.
If several servers share a system, call `disable_signals()` consistently and
stop the appropriate server controllers yourself. Do the same if your
application installs its own signal policy.

Fatal-signal notifications require runtime fatal signal handling
(`Builder::fatal_signals(true)` or `#[ntex::main(fatal_signals = true)]`), and
application-panic notifications require runtime panic handling. Both are off
by default.

Fatal-signal shutdown is best effort. The operating system may terminate the
process before graceful cleanup finishes. On Windows, Ctrl-C arrives as the
interrupt event.

The graceful server timeout bounds a worker as a whole. Each connection has
its own `IoConfig::set_shutdown_timeout()` for flushing and closing. Leave
enough server-level time for those connection shutdowns to complete.

## Connection and protocol configuration

Use `SharedCfg` to give each connection the configuration its I/O layer and
protocol services need. Pass it to `bind()` or `listen()`. ntex clones it onto
each accepted `Io`, and each layer retrieves the types it understands from
that container:

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

`IoConfig` controls connection-level behavior, including shutdown, memory,
and read-rate limits. `HttpServiceConfig` controls HTTP behavior, including
keep-alive, request-head limits, and protocol timeouts. TLS and other protocols
add their own configuration types to the same `SharedCfg`.

If you omit a configuration type, lookups fall back to its `Default` value.
Each worker thread creates and caches its own default. A lookup does not
report whether the value was added or defaulted. The framework's
configuration types get their defaults this way.

If your application requires a configuration value, add it explicitly.
Validate it when you build the `SharedCfg` rather than relying on a default.

The [I/O chapter](7-io.md) explains buffer limits, read rates, write
backpressure, keep-alive, and shutdown timing in detail.

## Test servers

Use `test_server()` to run a service on an available local port in a test.
It starts one worker on a separate thread and disables signal handling.
Its service factory receives `&()`, just like the factories passed to
`http::test::server` and `web::test::server`:

```rust
use ntex::http::{HttpService, Response};
use ntex::{client::Client, server};

#[ntex::test]
async fn test_server_response() {
    let server = server::test_server(async |_| {
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

Use `addr()` to get the address for an HTTP client. Use `connect()` to open
a client `Io` with the test server's client configuration. The `server()`
method returns the underlying server controller.

With `TestServerBuilder`, you can set separate server and client `SharedCfg`
values. If service construction needs application state, use
`TestServerBuilder::with(cfg, async |state| service)`. The service factory
receives `&Cfg::State`. The state-initialization closure remains
`async || Ok(state)`.

For custom listeners, `build_test_server()` exposes the full `ServerBuilder`.
Set the address on the returned `TestServer` before calling `connect()`.

Dropping the last `TestServer` clone stops its server and runtime. It briefly
blocks the current thread while the test thread shuts down. Drop it outside
timing-sensitive assertions.
