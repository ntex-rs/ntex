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
* `workers()` sets the worker count. It defaults to the number of logical CPUs
  the process may use, across all processor groups on Windows.
* `backlog()` sets the listen backlog for listeners created afterward.
* `max_connections()` sets the process-wide per-worker connection limit.
* `affinity()` sets the CPU placement of workers, see
  [Worker placement](#worker-placement). The default is `Affinity::Soft` on
  Windows and `Affinity::Core` elsewhere.
* `load_balancing()` selects how new connections are distributed between
  workers, see [Connection load balancing](#connection-load-balancing). The
  default `LoadBalancing::IncomingCpu` sends a connection to the worker on the
  CPU that receives its network packets unless that worker is busier than
  others, `LoadBalancing::LeastLoaded` to the less loaded of two random
  workers, and `LoadBalancing::RoundRobin` to the workers in turn.
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

`Server::status()` returns the server status, and `Server::status_changed()`
waits for the next change, e.g. to connect readiness to a supervisor or health
check. The server is `Starting` until a worker becomes available, then `Ready`.
It is `Unavailable` while no worker is available, `Paused` after
`Server::pause()` until `Server::resume()`, and `Stopping` and then `Stopped`
when it stops. Listeners accept connections only while the server is `Ready`.
The status describes acceptance state, not whether every worker is healthy.
Changes in quick succession may be reported once, with the latest status.

## Worker placement and load balancing

Two settings decide where a connection is processed. `affinity()` places each
worker thread on a CPU, and `load_balancing()` chooses the worker for each new
connection. With the defaults, a connection is handled on the CPU, or at least
the physical core, that receives its network packets. The packet data, the
socket state and the worker's own data then stay in the same CPU caches and
on the same NUMA node, and the network stack does not have to wake a thread
on another CPU for every received packet.

```rust,no_run
use ntex::server::{Affinity, LoadBalancing};

let builder = ntex::server::build()
    .workers(8)
    .affinity(Affinity::Core)
    .load_balancing(LoadBalancing::IncomingCpu);
```

### Worker placement

When the server starts, ntex reads the CPU topology and orders the logical
CPUs the process may use:

1. On hybrid CPUs, faster cores come first: performance cores before
   efficiency cores.
2. Within each class, one logical CPU of every physical core, interleaved
   across the L3 caches of a NUMA node and then across NUMA nodes. With four
   workers on a two-socket machine, each socket gets two workers, on
   different L3 caches if the socket has several.
3. Then the remaining SMT siblings (hyper-threads), in the same order.

Worker `i` takes entry `i` of that list, and the list starts over when there
are more workers than CPUs. Workers never share a physical core until every
core has a worker. A restarted worker keeps the placement of the worker it
replaces.

The placement is applied with one of the `Affinity` modes:

| Mode | Windows | Linux and Android |
|------|---------|-------------------|
| `Disabled` | not placed | not placed |
| `Cpu` | pinned to one logical CPU (`SetThreadGroupAffinity`) | pinned to one logical CPU (`sched_setaffinity`) |
| `Core` | pinned to the logical CPUs of one physical core | pinned to the logical CPUs of one physical core |
| `Soft` | preferred CPU only, the ideal processor (`SetThreadIdealProcessorEx`) | same as `Core` |

The default is `Soft` on Windows and `Core` on Linux and Android. `Core` lets
the scheduler move a worker between the SMT siblings of its core, which share
its L1 and L2 caches. Other platforms have no topology support and ignore
affinity.

Placement is applied on the worker thread before the worker's runtime is
created. The runtime, its timers and the worker's buffer page cache are
therefore allocated on the NUMA node of the worker's CPU. Each placement is
logged at `info` level:

```text
Set affinity to Soft cpu 2 (core 2 3) for worker "api:worker:1"
```

`Worker::start()` takes a logical CPU index for workers started directly. On
Windows, the CPUs of processor group 1 are numbered after those of group 0.

#### Windows

* The topology comes from `GetLogicalProcessorInformationEx`: physical cores
  with their efficiency class, L3 caches and NUMA nodes, across all processor
  groups.
* Windows splits machines with more than 64 logical CPUs into processor
  groups, and a thread runs in one group at a time.
  `std::thread::available_parallelism()` counts only the current group, ntex
  counts all of them for the default number of workers and spreads the
  workers over all groups. A `Soft` worker whose ideal processor is in another
  group also gets the affinity of that whole group, because the ideal
  processor must be in the thread's group.
* The CPUs the process may use come from an explicit process affinity mask
  (`start /affinity`, job object affinity limits), which also keeps the
  workers in the process's processor group, the group affinity of a job
  object the process runs in, and the default CPU sets of the process
  (`SetProcessDefaultCpuSets`, container CPU limits). The processor groups
  the process happens to run in are not a restriction, so without these all
  groups are used. Only Windows 10 APIs are used.
* `Soft` is a hint. The scheduler runs the worker on its ideal processor when
  that CPU is free, and on another CPU of the group otherwise, so a worker is
  not stuck behind a busy thread or interrupt-heavy CPU. Use `Core` or `Cpu`
  on dedicated machines when strict isolation matters more.

#### Linux and Android

* The topology comes from sysfs, `/sys/devices/system/cpu/cpuN/`:
  `topology/core_cpus_list` (or `thread_siblings_list`) for the physical
  core, `cache/index3/shared_cpu_list` for the L3 cache (the package if there
  is none), the `nodeN` link for the NUMA node, and `cpu_capacity` for
  big.LITTLE cores.
* The CPUs the process may use come from `sched_getaffinity()`, so `taskset`,
  cgroup cpusets (`docker run --cpuset-cpus`) and systemd's `CPUAffinity=`
  are honored.
* Linux has no preferred-CPU hint, so `Soft` works like `Core`.
* The default number of workers follows
  `std::thread::available_parallelism()`, which also honors a cgroup CPU
  quota (`docker run --cpus`). A quota does not restrict which CPUs run the
  process, so the workers are still spread over all allowed CPUs.

### Connection load balancing

The accept thread accepts a connection, chooses a worker and queues the
connection to it. Items submitted with `Server::process()` are dispatched the
same way, on the submitting thread. While no worker is available, items go to
the server's command loop, which runs on the thread that started the server,
and wait there until a worker is ready. The worker turns the socket into an
ntex `Io` and registers it with its own runtime, so all later I/O of the
connection runs on that worker.

Only workers that are ready receive connections, see
[Workers and service factories](#workers-and-service-factories). Among them,
`LoadBalancing` decides:

* `RoundRobin` sends connections to the workers in turn, skipping workers
  blocked by a task while another worker is available.
* `LeastLoaded` picks two random workers and takes the one with the lower
  runtime utilization, the share of time the worker's runtime was busy,
  averaged over about 250 ms and sampled at most every 10 ms. If the two
  differ by less than 10%, the one with fewer open and queued connections
  wins. Workers blocked by a task are skipped while another worker is
  available, see [Stall detection](#stall-detection). On tokio, busy time is
  available only for current-thread runtimes, otherwise the connection count
  decides.
* `IncomingCpu`, the default, prefers the worker on the CPU that receives the
  connection's packets, and works like `LeastLoaded` otherwise.

For each connection, `IncomingCpu`:

1. asks the operating system which CPU receives the connection's packets,
   once, when the connection is accepted;
2. finds the workers placed on that CPU, or, if none is, the workers whose
   physical core contains it. This includes SMT siblings the process may not
   use, for example with `taskset`, because the network stack may still
   process packets on them;
3. takes the one that is not blocked and has the lowest utilization, then
   the fewest connections;
4. compares it with the `LeastLoaded` choice, and uses it unless its
   utilization is more than 20% higher or its load exceeds the other worker's
   load by more than half plus 16 connections.

The limits keep the preference from overloading workers when the network
adapter delivers traffic to only a few CPUs, when its hash spreads
connections unevenly, or when some connections are much more expensive than
others. Under normal load nearly every connection stays local.

Connections without an incoming CPU use the `LeastLoaded` choice. This
covers loopback connections on Windows, platforms other than Windows, Linux
and Android, `Affinity::Disabled`, and custom `ServerConfiguration`
implementations that do not override `incoming_cpu()`. A worker whose
placement could not be applied is never preferred as local. Connections that
arrive while no worker is available wait in the server and are balanced the
same way once a worker becomes available.

The incoming CPU is read once. If the operating system later moves the
connection's traffic to another CPU, for example when RSS is rebalanced or
irqbalance moves an interrupt, the connection stays on its worker.

`ServerMetrics::incoming_cpu_hits` and `incoming_cpu_misses` show how well
this works, see [Incoming CPU](#incoming-cpu).

#### Rescheduling

A connection waits in its worker's queue until the worker takes it. The
server moves queued connections to other workers when their worker:

* fails, for example its service panics. Connections that are queued while
  no other worker is available wait in the server for the restarted worker;
* is not ready, for example its service is re-created;
* is blocked by a task, see [Stall detection](#stall-detection). With two or
  more workers, the server checks for blocked workers every 50 ms and moves
  their queued connections while another worker is not blocked.

Only queued connections move. A connection the worker has taken is
registered with that worker's runtime and stays there, a blocked worker
handles it once it is unblocked, and a failed worker drops it. Moved
connections are balanced again without their incoming CPU, and are not
counted in `incoming_cpu_hits` and `incoming_cpu_misses` again.
`ServerMetrics::rescheduled` counts them, see
[Rescheduled connections](#rescheduled-connections).

#### Windows: receive-side scaling

A network adapter with receive-side scaling (RSS) hashes each connection's
addresses and ports into an indirection table that selects a receive queue,
and each queue is served by one CPU, where NDIS runs the receive processing.
ntex reads that CPU with `WSAIoctl(SIO_QUERY_RSS_PROCESSOR_INFO)`, which
returns its processor group and number.

* With `Affinity::Soft`, the ideal processor of the chosen worker is the RSS
  CPU, so the receive processing that completes the worker's I/O and the
  worker thread it wakes usually run on the same CPU.
* By default, RSS uses one logical CPU per physical core, starting at the
  adapter's base processor. Worker placement also takes one logical CPU of
  every core first, so with a worker per core the RSS CPUs have their own
  workers.
* Loopback connections (`127.0.0.1`, `::1`) do not pass through an adapter
  and have no RSS CPU. Connections to one of the machine's own adapter
  addresses, including connections from WSL, do have one, which is useful to
  test this on a single machine. Adapters without RSS, or with RSS disabled,
  report none.
* `Get-NetAdapterRss` shows the adapter's RSS settings, including
  `BaseProcessorNumber`, `MaxProcessors` and the processors the indirection
  table uses. If many connections miss, align the RSS processors with the
  worker CPUs, for example:

```powershell
Set-NetAdapterRss -Name "Ethernet" -BaseProcessorNumber 0 -MaxProcessors 8
```

#### Linux and Android: `SO_INCOMING_CPU`

ntex reads `getsockopt(SOL_SOCKET, SO_INCOMING_CPU)` (Linux 3.19 or later),
the CPU on which the network stack processed the last packet received for
the socket. For a newly accepted connection, that is the CPU that processed
the last packet of the TCP handshake.

* That CPU is set by the interrupt affinity of the adapter's receive queue
  (`/proc/irq/N/smp_affinity_list`), and changed by Receive Packet Steering
  (`/sys/class/net/<dev>/queues/rx-N/rps_cpus`), which moves packet
  processing to other CPUs, and by Receive Flow Steering, which moves it to
  the CPU where the consuming thread last ran.
* Loopback packets are processed on the sending CPU, so loopback connections
  report the client's CPU. Test servers on loopback therefore count hits and
  misses too.
* `ethtool -l <dev>` shows the number of receive queues, `ethtool -L <dev>
  combined N` changes it, and `/proc/interrupts` shows which CPUs handle the
  queues. For the best locality, use one queue per worker and pin the queue
  interrupts to the worker CPUs. irqbalance may move them again, unless it is
  stopped or told to leave those interrupts alone.

### Choosing the settings

* The defaults suit most servers, including servers without a multi-queue
  network adapter, where `IncomingCpu` behaves like `LeastLoaded`.
* Use `LoadBalancing::LeastLoaded` if per-worker `active_connections` stay
  uneven under `IncomingCpu`, for example when an adapter has fewer queues
  than the server has workers and its CPUs carry most connections.
* On machines shared with other busy processes, prefer `Affinity::Soft` on
  Windows, or `Affinity::Disabled`, so the scheduler can move workers away
  from busy CPUs.
* With more workers than CPUs, several workers share a CPU, and
  `IncomingCpu` chooses the least loaded of them.
* `LoadBalancing::RoundRobin` distributes connections the way earlier ntex
  versions did, which keeps tests that count connections per worker
  predictable.

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

### Incoming CPU

These fields are on `ServerMetrics`, see `LoadBalancing::IncomingCpu`.

| Metric | Kind | Meaning |
|--------|------|---------|
| `incoming_cpu_hits` | counter | Connections sent to the worker on the CPU that receives their packets. |
| `incoming_cpu_misses` | counter | Connections with a known incoming CPU sent to another worker, because no worker is placed on that CPU or its worker was busier. |

Connections without a known incoming CPU, e.g. loopback connections on
Windows or servers on platforms other than Windows, Linux and Android, are
counted in neither. Many misses mean the network adapter delivers packets
to CPUs without workers, for example CPUs outside the process affinity, or
that the workers on the receiving CPUs are busier than the others, for
example the adapter uses fewer queues than the server has workers. Check
the adapter's RSS settings (`Get-NetAdapterRss` on Windows, `ethtool -l`
and `/proc/interrupts` on Linux) against the worker placement.

### Rescheduled connections

This field is on `ServerMetrics`, see
[Connection load balancing](#rescheduling).

| Metric | Kind | Meaning |
|--------|------|---------|
| `rescheduled` | counter | Connections taken from the queue of a failed, not ready or blocked worker and sent to another worker. |

An occasional increase follows a worker restart or a stall. A steady
increase means workers are blocked often, check `latency.stalls` and
`current_task` in [Runtime](#runtime) for the blocking code.

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
| `runtime` | | Runtime statistics of the worker thread, see below. |

* Compare `active_connections` and the rate of `items` across workers to
  check load balancing. With the default `LoadBalancing::IncomingCpu` or with
  `LoadBalancing::LeastLoaded`, they should be close. A worker with much
  higher `not_ready_duration` than the others points to long-lived or
  expensive connections pinned to it.
* If `not_ready_duration` grows on all workers together with
  `accept_paused_duration`, the server is at capacity.
* `active_connections` at the `max_connections()` value with idle CPU
  means the limit is too low for the workload, for example with many idle
  keep-alive or WebSocket connections.
* `restarts` should stay at zero. Any increase means a worker failed;
  alert on it and check the logs.

### Runtime

`WorkerMetrics::runtime` is a `RuntimeMetrics` with the statistics of the
worker thread's runtime. A field is `None` when the runtime does not provide
it. Totals are kept when a worker restarts; `current_turn`,
`current_task` and the `latency` gauges (`last`, `last_sample`, `stalled`)
describe the current worker thread.

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

`latency` is a `LatencyMetrics`. A task on each worker sleeps for
`interval` and records how late it wakes up:

| Metric | Kind | Meaning |
|--------|------|---------|
| `interval` | setting | Sampling interval. |
| `last` | gauge | Lateness of the most recent sample. |
| `histogram` | histogram | Lateness of all samples. |
| `last_sample` | gauge | Time of the most recent sample. |
| `stalls` | counter | Times the worker recorded no sample for longer than `interval` plus the stall threshold. |
| `stall_duration` | duration | Total duration of finished stalls. |
| `stalled` | gauge | Whether the worker is stalled now. |

On tokio, `busy_duration` is tokio's worker busy time, which excludes
handling I/O events.

* Utilization is the share of time the worker was busy, see
  [Worker utilization](#worker-utilization). A worker close to 100% is CPU
  bound: add workers or cores. Uneven utilization across workers points to
  expensive connections pinned to a few workers.
* Scheduling latency is what every request on the worker waits in addition
  to its own processing. Watch high percentiles of the `histogram`
  difference; values of several milliseconds mean the worker is overloaded
  or a task runs too long between `.await` points.
* Long turns in `poll_histogram` delay every task woken during the turn.
  They come from tasks that run too long between `.await` points; move such
  work to `spawn_blocking()` or split it.
* A ratio of `polls_again` to `polls` close to 1 means tasks are scheduled
  faster than the worker runs them, the worker has a backlog. A high `self_wakes` rate points
  to tasks that busy-loop by yielding.
* `stalls` should stay at zero. A stall means blocking code, such as a
  synchronous call or a lock, held the worker. Alert on it, see
  [Worker stalls and busy loops](#worker-stalls-and-busy-loops).

The sampling interval and the stall threshold are system builder settings,
see [Worker stalls and busy loops](#worker-stalls-and-busy-loops).

#### Worker utilization

`busy_duration` is a running total, kept across worker restarts. To get
utilization, take two snapshots and divide the growth of `busy_duration` by
the time between them:

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

* Use the same window for all workers. 5 to 10 seconds smooths out short
  spikes; shorter windows show bursts.
* On neon and compio `busy_duration` includes the turn that is running now,
  so a worker held by a long task shows up as 100% busy right away.
* On tokio, `busy_duration` is reported only for current-thread runtimes.
  It excludes handling I/O events, so values are lower than on neon or
  compio for the same load. Tokio adds busy time only after a batch of tasks finishes,
  so a stalled worker can show low utilization; treat a worker with
  `latency.stalled` set as fully busy, as the example does.
* Compare utilization with the rate of `items` and `active_connections`. High
  utilization with few connections points to expensive requests; high
  utilization on one worker only points to an imbalance, see the bullets
  above.

### I/O metrics

I/O metrics are collected per I/O configuration (`IoConfig` in a
`SharedCfg`) on each worker thread. Every listener configuration is
registered automatically, so `WorkerMetrics::io` has one `IoConfigMetrics`
entry per listener configuration, identified by `id`, `tag` and `service` of
the `SharedCfg`. Other configurations, for example the configuration of an
HTTP client used by the application, get their own entry when created with
`IoConfig::set_metrics(true)`. Connections whose configuration has no
metrics of its own are counted together in `io_other`. I/O metrics are
always collected: `set_metrics(false)`, the default, only removes the
separate entry of a configuration, and does not affect the registered entries
of listener configurations.

To get the totals of a service across the server, add up its entries from
all workers.

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

Backpressure time is connection-time: ten connections under backpressure
for one second add ten seconds. Divide its rate by the number of active
connections to get the share of time a connection spends blocked.

* **Throughput.** Rates of `bytes_read` and `bytes_written` give the
  bandwidth per service and worker. Divided by the `items` rate they give
  the average traffic per connection.
* **Memory.** `read_buffer_capacity` plus `write_buffered` is the memory
  held in connection buffers. If it is high, lower `set_read_backpressure()`
  and `set_write_backpressure()`, or lower `max_connections()`.
* **Partial input.** A read buffer is released once its input is consumed,
  so busy connections are counted in `read_buffers_held` only briefly; a
  snapshot mostly shows idle connections waiting for the rest of a frame.
  Each keeps a whole page for a few bytes, so `read_buffer_capacity` far
  above `read_buffered` means memory is held for small leftovers. Many such
  connections point to slow or stalled clients; `set_frame_read_rate()`
  bounds how long they can hold a buffer, and a smaller minimum page in
  `set_read_size()` lowers the cost of each. Buffers held by a pending
  io-uring read, and frames split off a buffer that keep its page alive, are
  not counted; [Io buffer attribution](#io-buffer-attribution) counts
  them.
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

### Io buffer attribution

The I/O metrics count only the read buffers a connection holds. Input that
leaves the connection is not counted: `Bytes` frames split off a read buffer
and kept by the application keep the whole buffer alive, and so do buffers
held by a pending io-uring read. Io buffer attribution charges each read
buffer allocation to a `BytesTracker` for as long as the allocation lives,
wherever its data ends up. It answers questions such as "how much memory do
request bodies of this listener hold" or "which connection holds the most
input".

Attribution needs the `tracker` feature, which adds a pointer to the header
of every heap buffer:

```toml
ntex = { version = "4", features = ["tracker"] }
```

It is configured per `IoConfig` with `set_buf_tracker()`:

| `BufTracker` | Behavior |
|------------------|----------|
| `Disabled` | Read buffers are not attributed. This is the default. |
| `PerConnection` | Each connection charges its read buffers to its own tracker, created when first used. |
| `Shared(tracker)` | All connections of the configuration, on all workers, charge their read buffers to `tracker`. |

A shared tracker gives a total per listener or service. Create it before
the server starts and keep a clone to read it:

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

With `PerConnection`, `IoRef::buf_tracker()` (and
`IoContext::buf_tracker()` in filters) returns the connection's tracker,
for example from `HttpRequest::io()` in a handler. It returns `None` when
attribution is disabled. The returned reference must not be used after
`Io::set_config()`; clone the tracker to keep it longer. A clone keeps
reporting buffers that outlive the connection, but does not keep the
connection alive.

```rust,ignore
if let Some(tracker) = req.io().and_then(|io| io.buf_tracker()) {
    if tracker.snapshot().live_bytes > 64 * 1024 * 1024 {
        // the connection holds too much input, reject or close it
    }
}
```

The tracker is not limited to the connection's own read buffers. The
application can charge its own buffers to it, so memory a connection causes
outside the I/O layer shows up in the same metrics, for example a body
assembled from many frames, a decompressed payload, or a buffer received from
another source on behalf of the connection. This works with `PerConnection`
and `Shared` trackers:

* `BytesMut::with_page_size_in(size, tracker)` takes a new buffer from the
  page cache, charged to `tracker`.
* `BytesMut::set_tracker(tracker)` charges an existing buffer. If the
  allocation is already charged to another tracker, the charge moves, also
  while `Bytes` frames split off it are still alive.
* `BytesMut::tracker()` returns the tracker a buffer is charged to.

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

Such buffers follow the same rules as read buffers: the charge lasts until
the last handle is dropped, a buffer that grows stays charged, and copies are
not charged.

`BytesTracker::snapshot()` returns a `BytesTrackerSnapshot`:

| Metric | Kind | Meaning |
|--------|------|---------|
| `live_bytes` | gauge | Memory held by the attributed allocations, including the allocation header. |
| `live_allocations` | gauge | Number of attributed allocations. |

The values are not part of `ServerMetrics` or `IoMetricsSnapshot`; read the
trackers you created and export them next to those metrics. The two fields
are read one after the other, so a snapshot taken while buffers are
allocated or released is only approximately consistent.
`BytesTracker::ptr_eq()` tells whether two handles refer to the same
tracker.

What is charged:

* Every read buffer the connection allocates or takes from the page cache,
  with its full allocation size, not the bytes of data in it. A buffer that
  grows into a new allocation stays charged to the same tracker.
* Buffers of a pending read, including io-uring provided-buffer pages once
  data is received into them. A buffer returned empty by a read goes back to
  the page cache and its charge ends.
* Plaintext read buffers of filters such as TLS.
* Frames split off a read buffer, until the last `Bytes` or `BytesMut`
  referring to the allocation is dropped. The charge ends on whichever
  thread drops it, also after the connection is closed.

What is not charged:

* Pages in the page cache. A cached page is charged again to its next user
  only.
* Write buffers.
* Copies of the data: small frames stored inline in the `Bytes` handle (up
  to 23 bytes on 64-bit targets), the result of `Bytes::trimdown()`, and
  anything the application copies into its own types, unless it charges
  them explicitly as described above. Cloning, splitting
  or freezing a buffer does not copy, so it stays charged.

When `Io::set_config()` changes the configuration of a connection, for
example after a protocol upgrade:

* With `PerConnection` in both, the connection keeps its tracker.
* Switching to `Shared(tracker)` moves the buffers the connection holds to
  `tracker`. Frames that already left the connection stay charged to the
  old tracker.
* Switching from `Shared` or `Disabled` to `PerConnection` creates a new
  tracker.
* Switching to `Disabled` stops charging new buffers; existing buffers stay
  charged until they are released.

Cost:

* Without the `tracker` feature there is no cost. With the feature, the
  buffer header grows from 16 to 32 bytes on 64-bit targets (20 bytes on
  32-bit targets), for every heap buffer, attributed or not.
* Cloning, splitting and freezing buffers do no tracker work. Allocating,
  growing and releasing the last handle of a buffer check for a tracker,
  and update its atomic counters if there is one.
* A `Shared` tracker is updated by every worker thread and becomes a
  contended cache line under load. Use one per listener or service rather
  than one per server, or `PerConnection` when only per-connection limits
  are needed.

Interpreting the values:

* **Escaped input.** `live_bytes` of a shared tracker well above the sum of
  `read_buffer_capacity` for the same service means the application keeps
  input after the connection is done with it: request bodies queued for
  later processing, frames stored in caches, or pending io-uring reads.
  Storing a small part of a large buffer keeps the whole allocation; copy
  it, for example with `Bytes::trimdown()`, to release the buffer.
* **Leaks.** `live_allocations` that keeps growing while the number of
  active connections is stable points to frames that are never dropped.
* **Per-connection cost.** `live_bytes / live_allocations` is the average
  allocation size, and `live_bytes` divided by `active_connections` the
  average input memory per connection. A high average with little
  traffic points to the partial-input case described in
  [I/O metrics](#io-metrics).
* **Abusive clients.** With `PerConnection`, check the connection's tracker
  before accepting more input, and close connections that exceed a limit.
  Unlike `set_read_backpressure()`, this also counts input the connection
  already handed to the application.

## Worker stalls and busy loops

Each worker runs its connections on a single thread. Code that blocks that
thread, such as a synchronous call, a lock held for a long time or a long
computation between `.await` points, delays every connection on the worker.
The runtime detects blocked workers and tasks that never wait, logs them,
and reports them in the worker metrics.

### Stall detection

Every worker runs a task that sleeps for the latency sampling interval and
records how late it wakes up. A watchdog thread checks the workers every
interval. A worker that has recorded no sample for longer than the interval
plus the stall threshold is stalled. With the defaults, 100 ms and 1 s, a
worker is stalled after about 1.1 s without progress.

When a stall starts, the watchdog logs an error:

```text
Arbiter my-server:worker:0(Id(3)) is stalled, no progress for 1.1s, running task spawned at src/main.rs:42:9
```

On neon the message names the spawn location of the task that is running.
Tasks spawned by ntex itself, such as the connection dispatchers, report a
location inside ntex. A stall without a running task happens in the I/O
driver. When the worker makes progress again, a warning reports the stall
duration:

```text
Arbiter my-server:worker:0(Id(3)) recovered after 2.4s stall
```

The same information is in `WorkerMetrics::runtime`:

* `latency.stalls` and `latency.stall_duration` count finished stalls.
  Alert on any increase.
* `latency.stalled` is set while the worker is stalled. `current_turn`
  shows how long the worker has been blocked and `current_task` where the
  blocking task was spawned (neon).
* Shorter blocking that stays under the threshold shows up as high values
  in the `latency.histogram` and `poll_histogram` differences.

New connections skip a worker whose runtime turn has lasted 100 ms or more,
that has recorded no latency sample for 100 ms past its interval, or that is
stalled, as long as another worker is available. Connections queued at the
blocked worker are moved to other workers, see
[Rescheduling](#rescheduling). Connections the worker has already taken
still wait for it.

Change the interval and the threshold, both in milliseconds, on the system
builder. `#[ntex::main]` has no options for them, build the system
directly:

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
histogram, at the cost of more timer wakeups on idle workers. Keep the
threshold above the longest blocking you accept, so it does not log
expected work. `latency_interval(0)` disables sampling and stall detection,
and `WorkerMetrics::runtime.latency` is then `None`.

### Busy loop detection

A task that wakes itself on every poll, for example a loop around
`yield_now()` or `wake_by_ref()`, never waits for an event. It keeps the
worker busy without blocking it, so it is not a stall, but it wastes CPU
and delays other tasks. The neon runtime counts these self-wakes. When a
task wakes itself 10 000 times in a row over at least one second, the
runtime logs a warning once for that streak:

```text
Task spawned at src/main.rs:42:9 woke itself 10000 times in 1.2s without waiting, possible busy loop
```

`WorkerMetrics::runtime.self_wakes` is the total count. A steadily rising
rate together with high utilization (`busy_duration`) and a low connection
rate points to a busy loop; replace it with code that waits for an event or
a timer. The tokio and compio runtimes do not count self-wakes. The limits
can be changed with `RuntimeBuilder::self_wake_threshold()` only when the
runtime is built directly, see the runtime chapter.

### Backtrace capture

On Linux and Windows the watchdog captures a backtrace of the stalled
worker thread once, when the stall starts:

* On Linux it sends `SIGUSR2` to the worker thread, whose handler records
  the stack. This requires ntex signal handling on the system,
  `signals = true` in `#[ntex::main]` or `System::build().signals(true)`.
  The application must not use `SIGUSR2` for anything else.
* On Windows the watchdog briefly suspends the worker thread and walks its
  stack, on x86_64 and aarch64.
* Other platforms only log the stall.

Without a callback the backtrace is resolved and logged as an error together
with the task location. `System::set_latency_callback()` replaces the
logging, for example to send the backtrace to an error tracker. The callback
is process-wide and runs on the watchdog thread, so keep it short; it
receives the unresolved backtrace and the spawn location of the blocking
task. The location is `None` when the task changed during the capture,
because the backtrace then may not belong to it.

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
wrong or missing function names; add `debug = "line-tables-only"` to the
release profile for readable backtraces with file and line numbers.

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
