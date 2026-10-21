# Changes

## [4.3.0] - 2026-10-21

* Move items queued at a failed, unavailable or blocked worker to other workers,
  add `ServerMetrics::rescheduled`

* `LoadBalancing::RoundRobin` skips blocked workers while another worker is available

* Add `Server::metrics()` with per-worker (`WorkerMetrics`), per-listener (`ListenerMetrics`)
  and accept-loop statistics (`ServerMetrics`)

* Add `WorkerMetrics::io`, I/O backpressure, buffered, read and written bytes, eager
  writes and close reasons of the worker's connections, one entry per listener
  configuration, and `WorkerMetrics::io_other` for the worker's other connections

* Add `WorkerMetrics::page_cache`, statistics of the worker thread's buffer page cache

* Add `WorkerMetrics::runtime` (`RuntimeMetrics`), the worker runtime's busy time,
  polls, self-wakes, poll durations, current turn and task, scheduling latency and
  stalls. Totals are kept across worker restarts

* Re-export `ServerMetrics`, `WorkerMetrics`, `ListenerMetrics` and `RuntimeMetrics`
  from `ntex_server::net`

* Add `WorkerPool::load_balancing()` and `ServerBuilder::load_balancing()`.
  `LoadBalancing::LeastLoaded` sends each item to the less loaded of two random
  workers, by runtime utilization, then open connections and queued items, and
  skips workers blocked by a task. `LoadBalancing::IncomingCpu`, the default,
  sends a connection to the worker placed on the CPU that receives its network
  packets (the RSS CPU on Windows, `SO_INCOMING_CPU` on Linux and Android),
  unless that worker is busier than the `LeastLoaded` choice, and works like
  `LeastLoaded` otherwise. Use `LoadBalancing::RoundRobin` for the previous
  behavior

* Add `ServerConfiguration::incoming_cpu()`, the incoming CPU of an item used by
  `LoadBalancing::IncomingCpu`, and `ServerMetrics::{incoming_cpu_hits,
  incoming_cpu_misses}`

* Add `WorkerPool::affinity()` and `ServerBuilder::affinity()` with
  `Affinity::{Disabled, Cpu, Core, Soft}`. `enable_affinity()` is deprecated
  and does nothing.
  Workers are placed on physical cores first, spread across NUMA nodes and L3
  caches, then on SMT siblings, instead of taking CPUs from the highest number
  down. `Soft` sets the Windows ideal processor without pinning, and is group
  aware. Affinity is now enabled by default: `Soft` on Windows, `Core` (pin to
  the CPUs of one physical core) on other platforms. Affinity is supported on
  Windows, Linux and Android, other platforms ignore it

* Remove the `core_affinity` dependency. `Worker::start()` takes the index of a
  logical CPU (`Option<usize>`) instead of `core_affinity::CoreId`

* The default number of workers counts the CPUs of all processor groups on
  Windows, it was limited to 64 before

* Worker affinity is set before the worker's runtime is created, so runtime
  memory is allocated on the NUMA node of the worker's CPU

* On Windows, worker placement honors the job object group affinity and the
  default CPU sets of the process

* `Server::process()` and the accept thread send items to workers on the calling
  thread, instead of passing them through the server's command loop. Connections
  are no longer delayed by tasks on the thread that started the server.
  `ServerConfiguration::incoming_cpu()` is called on the submitting thread

* A stop caused by a worker failure with `stop_on_panic` also stops the other
  workers

* Use the `ntex-polling` fork of `polling` for the accept loop, the same crate as
  `ntex-net`

* Fix a data race between workers cloning a service factory that is `Send` but
  not `Sync`, each worker gets its own copy of the factory

* Stop all workers when the server stops, including workers that are not ready
  or still creating their service

* Add `Server::status()` and `Server::status_changed()`. `ServerStatus` reports
  `Starting`, `Ready`, `Paused`, `Unavailable`, `Stopping` and `Stopped`, it replaces
  the previous `Ready` and `NotReady` statuses and is exported from the crate root

* Remove `ServerBuilder::status_handler()` and `AcceptLoop::set_status_handler()`,
  use `Server::status_changed()`

* `Server::process()` accepts items only while the server is `Ready`. A server paused
  with `Server::pause()` stays paused when workers become available or restart, and
  resuming a server without available workers no longer resumes the listeners

* Fix `Worker::wait_for_status()` missing status changes when several clones of a
  worker wait at the same time. Each clone now observes every change

* Fix the signal handling task keeping a stopped server and its factory alive

* Fix a deadlock when `ServerConfiguration::incoming_cpu()` submits an item to the same
  server. The callback no longer runs under the server's routing lock, an item submitted
  while the callback runs, by itself or by another thread, is sent without its CPU

* Fix `Server::process()` returning `Ok` for an item submitted while the server starts
  stopping. The item was dropped, now it is returned as `Err`

## [4.2.0] - 2026-10-01

* Fix worker restart after a service panic on runtimes that catch task panics (compio, tokio);
  avoid process abort on compio when a worker panics

* Rename `ServerBuilder::maxconn()` to `max_connections()`; the old name is deprecated

* Replace `ntex-polling` with the upstream `polling` crate

* Rename `WorkerPool::shutdown_timeout()` and `ServerBuilder::shutdown_timeout()`
  to `graceful_shutdown_timeout()`, to distinguish the worker level timeout from
  the per connection `IoConfig::set_shutdown_timeout()`

* Remove `ServerStatus::WorkerFailed`, it was never reported

* Fix awaiting a `Server` that hangs when it is awaited during or after the stop,
  including a stop caused by a worker failure with `stop_on_panic`

## [4.1.1] - 2026-09-18

* Api docs improvements

## [4.1.0] - 2026-09-14

* Move ServerAppConfig to root

* Rename `build_with_config()` helper

## [4.0.0] - 2026-09-14

* Refactor server state creation process

## [4.0.0-beta.1] - 2026-08-26

* Shutdown on SIGTERM is always graceful

## [4.0.0-beta.0] - 2026-08-24

* Add app state management

* Migrate to ntex-service 5

## [3.11.0] - 2026-08-09

* Stop server when one of worker panics (stop_on_panic)

* Allow to enable graceful shutdown for signals

## [3.10.4] - 2026-06-22

* Fix lost of wakes in accept loop, partial fix for #911

## [3.10.3] - 2026-06-22

* Do not immediately retry ErrorKind::InvalidInput

## [3.10.2] - 2026-06-19

* Do not delay server stop for testing mode

## [3.10.1] - 2026-06-19

* Fix server ready notification

* Better signals handling

## [3.10.0] - 2026-06-15

* Move signals management to ntex-rt

## [3.9.0] - 2026-02-17

* Use new ntex-rt api

## [3.8.0] - 2026-02-16

* SharedCfg is not Copy

## [3.7.0] - 2026-02-12

* Allow to set multiple ServiceConfig::on_worker_start() callbacks

## [3.6.0] - 2026-01-28

* Use system name for workers

## [3.5.0] - 2026-01-03

* Use updated System api

## [3.4.0] - 2025-12-17

* Upgrade to ntex-service v4

* Improve test servers shutdown process

## [3.3.0] - 2025-12-05

* Use AsyncFn for on-accept callback

* Use AsyncFn for on-worker-start callback

## [3.2.0] - 2025-12-04

* Stop testing server faster

## [3.1.0] - 2025-12-03

* ServiceRuntime::service() and ServiceRuntime::config() returns self

## [3.0.0-pre.3] - 2025-12-01

* Use async fn for test server

## [3.0.0-pre.2] - 2025-11-30

* Fix config usage for registered service

## [3.0.0-pre.1] - 2025-11-28

* Use async factories for server configuration

## [3.0.0-pre.0] - 2025-11-27

* New configuration subsystem

## [2.8.1] - 2025-05-27

* Clear events list for accept loop

## [2.8.0] - 2025-04-08

* Add ServerBuilder::on_accept() callback

## [2.7.3] - 2025-03-28

* Better worker availability handling

## [2.7.2] - 2025-03-27

* Handle paused state

## [2.7.1] - 2025-02-28

* Fix set core affinity out of worker start #508

## [2.7.0] - 2025-01-31

* Cpu affinity support for workers

## [2.6.2] - 2024-12-30

* Fix error log

## [2.6.1] - 2024-12-26

* Tune shutdown logging

## [2.6.0] - 2024-12-04

* Use updated Service trait

## [2.5.0] - 2024-11-04

* Use updated Service trait

## [2.3.0] - 2024-07-16

* Add Server to TestServer

## [2.2.0] - 2024-07-16

* Expose TetServer::stop() method

## [2.1.0] - 2024-06-27

* Shutdown service on error and on worker shutdown

## [2.0.0] - 2024-05-28

* Use async fn for Service::ready() and Service::shutdown()

## [1.0.5] - 2024-04-02

* Fix external configuration handling

## [1.0.4] - 2024-03-30

* Fix signals support #324

## [1.0.3] - 2024-03-29

* Fix windows signals support #322

## [1.0.2] - 2024-03-25

* Add basic signals support

## [1.0.1] - 2024-03-24

* Re-add Server::build() method

## [1.0.0] - 2024-03-24

* Release
