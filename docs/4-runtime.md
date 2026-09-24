# Runtime

ntex is built around a single-threaded execution model. Tasks stay on the
thread where they were started, so many ntex types do not need to implement
`Send` or `Sync`.

This does not limit an ntex application to one CPU core. ntex can start several
worker threads, with each worker running its own single-threaded runtime.
Within a worker, tasks and services can safely use types such as `Rc`, `Cell`,
and `RefCell` without cross-thread synchronization.

State shared between workers must still use thread-safe types such as `Arc`,
atomics, or locks.

ntex keeps its networking and service APIs separate from the task runtime and
network reactor that drive them. [`DefaultRuntime`] selects compatible
implementations for both. The available backend families are:

- Tokio
- Compio
- the native ntex runtime and platform reactor

The backend is selected through Cargo features. `tokio` takes precedence if
both `tokio` and `compio` are enabled; applications should normally enable at
most one runtime feature.

## Tokio

Enable the `tokio` feature to run ntex on Tokio:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio"] }
```

Each arbiter runs a Tokio current-thread runtime with a `LocalSet`. If a Tokio
runtime is already active on the thread, ntex reuses it instead of creating a
new one. Because ntex tasks run inside Tokio, crates built on Tokio, such as
`tokio::time`, `tokio::sync`, or Tokio-based clients, can be used directly from
ntex services.

## Compio

Enable the `compio` feature to use Compio:

```toml
[dependencies]
ntex = { version = "4", features = ["compio"] }
```

Each arbiter runs its own Compio runtime, and Compio chooses the I/O driver
for the platform, such as io_uring on Linux or IOCP on Windows. The native
`neon-*` reactor features have no effect with this backend. Because ntex tasks
run inside the Compio runtime, other Compio-based crates can be used from ntex
services.

## Native runtime

When neither `tokio` nor `compio` is enabled, ntex uses its native runtime. It
selects an I/O reactor based on the current platform:

- On Linux, it tries `io_uring` first and falls back to a polling reactor when
  `io_uring` is unavailable.
- On other Unix platforms, it uses the polling reactor.
- On Windows, it uses IOCP.

You can select a specific native reactor through a Cargo feature:

- `neon-polling` selects the polling reactor.
- `neon-uring` selects the `io_uring` reactor on Linux.
- `neon-iocp` selects the IOCP reactor on Windows.

The reactor-selection features are intended to be mutually exclusive.

For example, to use the polling reactor:

```toml
[dependencies]
ntex = { version = "4", features = ["neon-polling"] }
```

To require `io_uring` on Linux:

```toml
[dependencies]
ntex = { version = "4", features = ["neon-uring"] }
```

The native runtime gives ntex direct control over task scheduling and its I/O
reactor. The Tokio and Compio backends are useful when integrating ntex into
applications that already use those ecosystems.

[`DefaultRuntime`]: https://docs.rs/ntex/latest/ntex/rt/struct.DefaultRuntime.html

## Spawning tasks

Use [`ntex::rt::spawn`] to start a future on the current runtime thread:

```rust
let task = ntex::rt::spawn(async {
    // `Rc`, `RefCell`, and other `!Send` values may be used here.
    10usize
});

assert_eq!(task.await.unwrap(), 10);
```

The future and its result do not need to implement `Send`, but they must be
`'static`. `spawn()` panics when called outside an active ntex runtime.

The returned [`JoinHandle`] can be awaited, canceled with `cancel()`, or
detached explicitly with `detach()`. Dropping the handle also detaches the task;
it does not cancel it.

To submit work from another thread, obtain an arbiter's runtime handle with
[`Arbiter::handle`] and call `spawn()` on that handle. Cross-thread futures and
their results must implement `Send`.

[`Arbiter::handle`]: https://docs.rs/ntex-rt/latest/ntex_rt/struct.Arbiter.html#method.handle
[`JoinHandle`]: https://docs.rs/ntex/latest/ntex/rt/struct.JoinHandle.html
[`ntex::rt::spawn`]: https://docs.rs/ntex/latest/ntex/rt/fn.spawn.html

## System and Arbiter

Two main types manage the ntex runtime:
[`System`](https://docs.rs/ntex-rt/latest/ntex_rt/struct.System.html) and
[`Arbiter`](https://docs.rs/ntex-rt/latest/ntex_rt/struct.Arbiter.html).

`System` manages the runtime as a whole. It stores the system configuration,
tracks arbiters, handles signals and shutdown, and owns shared runtime services
such as the blocking thread pool.

An `Arbiter` represents one execution thread. Each arbiter owns an event loop
that runs local asynchronous tasks. Creating an arbiter starts a new
operating-system thread with its own single-threaded runtime.

You can access the current arbiter from code running on its thread:

```rust
let arbiter = ntex::rt::Arbiter::current();
```

`Arbiter::new()` starts another execution thread in the current system.
`Arbiter::stop()` requests that its event loop stop, and `join()` waits for an
arbiter-owned thread to exit. The system's primary arbiter runs on the thread
that started the system, so it has no thread handle and `join()` on it returns
`Ok(())` immediately.

A simple way to think about the two types is:

- `System` manages the complete runtime environment.
- `Arbiter` manages one execution thread within that system.

Building a system returns a [`SystemRunner`]. `block_on()` runs a root future
and stops when that future completes. `run_until_stop()` runs the event loop
until code calls `System::stop()` or `System::stop_with_code()`.

```rust
let result = ntex::rt::System::build()
    .name("worker")
    .build(ntex::rt::DefaultRuntime)
    .block_on(async {
        10usize
    });

assert_eq!(result, 10);
```

[`SystemRunner`]: https://docs.rs/ntex-rt/latest/ntex_rt/struct.SystemRunner.html

## Custom Runners

You can customize how the system's root future is driven by implementing the
[`Runner`](https://docs.rs/ntex-rt/latest/ntex_rt/trait.Runner.html) trait:

```rust
trait Runner: Send + Sync + 'static {
    fn block_on(&self, fut: BlockFuture) -> Result<(), Box<dyn Any + Send>>;
}
```

Pass the runner to the system builder:

```rust
let system = ntex::rt::System::build()
    .name("my-system")
    .build(MyRunner::new());
```

The system stores the runner in its configuration. The same runner drives the
main system thread and each runtime created for a new arbiter.

A custom runner does not change the task-spawning or network-reactor
implementation selected at compile time. Functions such as `ntex::rt::spawn()`
use the backend chosen through Cargo features.

For example, when the `tokio` feature is enabled, `spawn()` uses the Tokio
backend. A custom runner must establish the runtime environment expected by
that backend. Applications performing network I/O must also install a
compatible ntex network reactor. `DefaultRuntime` handles both requirements
and is appropriate unless the application needs specialized integration.

## Blocking Work

Each arbiter is single-threaded. If one task blocks its thread, every other task
on that arbiter must wait. CPU-intensive work and blocking system calls should
not run directly inside asynchronous tasks.

Use [`spawn_blocking()`](https://docs.rs/ntex-rt/latest/ntex_rt/fn.spawn_blocking.html)
to move this work to the system's blocking thread pool:

```rust,ignore
let result = ntex::rt::spawn_blocking(|| {
    // Perform CPU-intensive or blocking work.
    expensive_computation()
})
.await?;
```

The asynchronous task can await the result without blocking other work on its
arbiter. The closure and its result must implement `Send`.

When no ntex `System` is active, `spawn_blocking()` runs the closure immediately
on the calling thread and returns an already completed future. It should
therefore normally be called from inside a running system.

Dropping the returned future prevents work that is still queued from starting,
but it cannot interrupt a closure that is already running. Use `detach()` when
the queued operation should continue even if its result is no longer needed.
Cancellation or a panic in the closure is reported as `BlockingError`.

You can configure the blocking thread pool through the system builder:

```rust
let system = ntex::rt::System::build()
    .thread_pool_limit(32)
    .thread_pool_recv_timeout(
        std::time::Duration::from_secs(30),
    )
    .build(MyRunner::new());
```

`thread_pool_limit()` sets the maximum number of blocking worker threads.
`thread_pool_recv_timeout()` sets how long an idle worker waits for more work
before stopping.

Keeping blocking work away from arbiter threads is important. A single blocking
operation can otherwise pause connection handling, timers, and every other
asynchronous task running on the same thread.

ntex provides basic support for detecting stalled arbiters through
[`Builder::ping_interval()`](https://docs.rs/ntex-rt/latest/ntex_rt/struct.Builder.html#method.ping_interval)
and
[`Builder::ping_threshold()`](https://docs.rs/ntex-rt/latest/ntex_rt/struct.Builder.html#method.ping_threshold).

Ping round-trip records are collected on all supported platforms and are
available through [`System::list_arbiter_pings`]. On Linux, when process
signal handling is enabled, exceeding the configured threshold also triggers
an attempt to capture a backtrace from the unresponsive arbiter. The system
sends `SIGUSR2` to the stalled thread and records the backtrace from a
`SIGUSR2` handler. ntex installs that handler on Unix whenever signal handling
is enabled, so applications that rely on `SIGUSR2` for their own purposes
should take this into account. Backtrace capture is diagnostic and
does not guarantee that every stall can be identified.

[`System::list_arbiter_pings`]: https://docs.rs/ntex-rt/latest/ntex_rt/struct.System.html#method.list_arbiter_pings

## The `#[ntex::main]` Attribute

The easiest way to start an ntex runtime is to annotate an asynchronous
function with `#[ntex::main]`:

```rust
#[ntex::main]
async fn main() {
    // Perform asynchronous work.
}
```

The attribute turns the asynchronous function into a synchronous entry point.
It creates a `System`, starts the selected runtime, and runs the function's
future until it completes.

The generated code is conceptually similar to this:

```rust
fn main() {
    ntex::rt::System::build()
        .name("main")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async {
            // Perform asynchronous work.
        });
}
```

The attribute supports several options:

- `name = "..."` sets the system name.
- `signals = true` or `false` enables or disables signal handling.
- `panic_handling = true` or `false` enables or disables panic handling.
- `ping_interval = N` sets the arbiter ping interval in milliseconds. Set it
  to `0` to disable arbiter pings.
- `rt = Type` selects a runtime runner that implements `Runner`.

For example, this system checks its arbiters every 250 milliseconds:

```rust
#[ntex::main(ping_interval = 250)]
async fn main() {
    // Perform asynchronous work.
}
```

You can combine several options:

```rust
#[ntex::main(
    name = "my-service",
    signals = true,
    panic_handling = true,
    ping_interval = 250,
)]
async fn main() {
    // Start the application.
}
```

You can also provide a custom runtime runner:

```rust
#[ntex::main(rt = MyRunner)]
async fn main() {
    // Perform asynchronous work.
}
```

For most applications, `#[ntex::main]` is the simplest way to create and run an
ntex system. Applications that need more control can build a `System` directly
with `System::build()`.

## The `#[ntex::test]` attribute

`#[ntex::test]` runs an asynchronous test inside a test-configured ntex
`System`:

```rust
#[ntex::test]
async fn runtime_test() {
    let value = ntex::rt::spawn(async { 10usize }).await.unwrap();
    assert_eq!(value, 10);
}
```

The generated test disables signal and panic handling. Unless the
`no-test-logging` feature is enabled, it also initializes ntex's test logging
support.
