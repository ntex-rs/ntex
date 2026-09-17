# Runtime

ntex is built around a strictly single-threaded execution model. Most of its
types do not implement `Send` or `Sync`, and asynchronous tasks remain on the
thread where they were started.

This does not mean that an ntex server can use only one CPU core. A server can
start multiple worker threads, with each worker running its own single-threaded
runtime. Within a worker, tasks and services can use types such as `Rc`, `Cell`,
and `RefCell` without cross-thread synchronization.

State that is deliberately shared between workers must still use thread-safe
types such as `Arc`, atomics, or locks.

ntex separates its networking and service abstractions from the runtime that
executes them. It currently supports three runtime backends:

- Tokio
- Compio
- Neon, the native ntex runtime

The backend is selected through Cargo features.

## Tokio

Enable the `tokio` feature to run ntex on Tokio:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio"] }
```

ntex still follows its single-threaded execution model when using Tokio. The
Tokio backend provides the underlying task scheduler and I/O driver.

## Compio

Enable the `compio` feature to use Compio:

```toml
[dependencies]
ntex = { version = "4", features = ["compio"] }
```

As with the Tokio backend, ntex services continue to run within a
single-threaded execution context.

## Neon

When neither `tokio` nor `compio` is enabled, ntex uses its native runtime. The
`neon` feature can also be enabled explicitly:

```toml
[dependencies]
ntex = { version = "4", features = ["neon"] }
```

By default, the native runtime chooses an appropriate I/O reactor for the
current platform:

- On Linux, it tries `io_uring` first and falls back to a polling reactor when
  `io_uring` is unavailable.
- On other Unix platforms, it uses the polling reactor.
- On Windows, it uses IOCP.

You can also select a reactor explicitly:

- `neon-polling` selects the polling reactor.
- `neon-uring` selects the `io_uring` reactor on Linux.
- `neon-iocp` selects the IOCP configuration on Windows.

For example, to use the polling reactor:

```toml
[dependencies]
ntex = { version = "4", features = ["neon-polling"] }
```

To use `io_uring` on Linux:

```toml
[dependencies]
ntex = { version = "4", features = ["neon-uring"] }
```

The native runtime gives ntex more control over the underlying I/O reactor,
while the Tokio and Compio backends make it easier to integrate ntex into
applications already using those ecosystems.

## System and Arbiter

Two main types manage the ntex runtime:
[`System`](https://docs.rs/ntex-rt/latest/ntex_rt/struct.System.html) and
[`Arbiter`](https://docs.rs/ntex-rt/latest/ntex_rt/struct.Arbiter.html).

`System` manages the runtime as a whole. It owns the system configuration,
tracks arbiters, handles shutdown and signals, and provides shared runtime
services such as the blocking thread pool.

An `Arbiter` represents an individual execution thread. Each arbiter owns an
event loop and provides the environment in which local asynchronous tasks run.
Creating a new arbiter starts a new operating-system thread with its own
single-threaded runtime.

The current arbiter can be accessed from within its thread:

```rust
let arbiter = ntex::rt::Arbiter::current();
```

An `Arbiter` handle can also be used to interact with that execution thread,
including stopping it when it is no longer needed.

A useful way to think about these types is:

- `System` manages the complete runtime environment.
- `Arbiter` manages one runtime thread within that system.

## Custom Runners

The runtime startup process can be customized through the
[`Runner`](https://docs.rs/ntex-rt/latest/ntex_rt/trait.Runner.html) trait:

```rust
trait Runner: Send + Sync + 'static {
    fn block_on(&self, fut: BlockFuture) -> Result<(), Box<dyn Any + Send>>;
}
```

A custom runner can be passed to the system builder. The same runner is then
used to drive the main system runtime and the runtimes created for its arbiters.

```rust
let system = ntex::rt::System::build()
    .name("my-system")
    .build(MyRunner::new());
```

There is one important limitation: a custom `Runner` controls how the runtime is
driven, but it does not replace the backend-specific task-spawning
implementation. Functions such as `ntex::rt::spawn()` are selected at compile
time through Cargo features.

For example, when the `tokio` feature is enabled, `spawn()` uses the Tokio
backend. A custom runner cannot switch task spawning to the Neon runtime at
run time. The selected feature, the runner, and the runtime environment must
therefore be compatible.

## Blocking Work

Because each arbiter is single-threaded, blocking its thread prevents every
other task on that arbiter from making progress. CPU-intensive work and blocking
system calls should not run directly inside an asynchronous task.

ntex-rt provides
[`spawn_blocking()`](https://docs.rs/ntex-rt/latest/ntex_rt/fn.spawn_blocking.html)
for this purpose:

```rust
let result = ntex::rt::spawn_blocking(|| {
    // Perform CPU-intensive or blocking work.
    expensive_computation()
})
.await?;
```

The closure runs on the system's blocking thread pool rather than on the
arbiter's event-loop thread. The asynchronous task can await the result without
blocking other work on the same arbiter.

The blocking pool can be configured through the system builder:

```rust
let system = ntex::rt::System::build()
    .thread_pool_limit(32)
    .thread_pool_recv_timeout(std::time::Duration::from_secs(30))
    .build(MyRunner::new());
```

`thread_pool_limit()` controls the maximum number of blocking worker threads.
`thread_pool_recv_timeout()` controls how long an idle thread waits for more
work before stopping.

Keeping blocking work away from arbiter threads is essential. A single blocking
operation can otherwise pause connection handling, timers, and every other
asynchronous task running on the same thread.

## The `#[ntex::main]` Attribute

The easiest way to start an ntex runtime is to annotate an asynchronous function
with `#[ntex::main]`:

```rust
#[ntex::main]
async fn main() {
    // Perform asynchronous work.
}
```

The attribute turns the asynchronous function into a regular synchronous entry
point. It creates an ntex `System`, starts the selected runtime, and runs the
function's future until it completes. Once the future finishes, the system is
stopped and its runtime resources are released.

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

The attribute accepts several options for configuring the system:

- `name = "..."` sets the system name.
- `signals = true` or `false` enables or disables signal handling.
- `panic_handling = true` or `false` enables or disables panic handling.
- `ping_interval = N` sets the arbiter ping interval in milliseconds. Set it to
  `0` to disable arbiter pings.
- `rt = Type` selects the runtime runner, which must implement `Runner`.

For example, the following system checks its arbiters every 250 milliseconds:

```rust
#[ntex::main(ping_interval = 250)]
async fn main() {
    // Perform asynchronous work.
}
```

Several options can be combined:

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

A custom runtime runner can also be supplied:

```rust
#[ntex::main(rt = MyRunner)]
async fn main() {
    // Perform asynchronous work.
}
```

For most applications, `#[ntex::main]` is the simplest way to create and run an
ntex system. Applications that need more control over runtime construction can
build a `System` directly with `System::build()`.
