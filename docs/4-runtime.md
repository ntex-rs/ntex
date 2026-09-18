# Runtime

ntex is built around single-threaded execution model. Tasks stay on the
thread where they were started, so many ntex types do not need to implement
`Send` or `Sync`.

This does not limit an ntex to one CPU core. ntex can start several
worker threads, with each worker running its own single-threaded runtime.
Within a worker, tasks and services can safely use types such as `Rc`, `Cell`,
and `RefCell` without cross-thread synchronization.

State shared between workers must still use thread-safe types such as `Arc`,
atomics, or locks.

ntex keeps its networking and service APIs separate from the runtime that
drives them. This separation allows ntex to support different runtimes. It
currently supports three runtime backends:

- Tokio
- Compio
- Neon

The backend is selected through Cargo features.

## Tokio

Enable the `tokio` feature to run ntex on Tokio:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio"] }
```

ntex keeps its single-threaded runtime when using Tokio. Tokio provides
the underlying task scheduler and I/O driver, while ntex tasks remain local to
the runtime thread where they were started.

## Compio

Enable the `compio` feature to use Compio:

```toml
[dependencies]
ntex = { version = "4", features = ["compio"] }
```

As with the Tokio backend, ntex services run within a single-threaded execution
context.

## Neon

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

The native runtime gives ntex direct control over its I/O reactor. The Tokio
and Compio backends are useful when integrating ntex into applications that
already use those ecosystems.

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

An `Arbiter` handle can also be used to interact with its execution thread or
stop it when it is no longer needed.

A simple way to think about the two types is:

- `System` manages the complete runtime environment.
- `Arbiter` manages one execution thread within that system.

## Custom Runners

You can customize how backend runtime is created by implementing the
[`Runner`](https://docs.rs/ntex-rt/latest/ntex_rt/trait.Runner.html) trait:

```rust
trait Runner: Send + Sync + 'static {
    fn block_on(
        &self,
        fut: BlockFuture,
    ) -> Result<(), Box<dyn Any + Send>>;
}
```

Pass the runner to the system builder:

```rust
let system = ntex::rt::System::build()
    .name("my-system")
    .build(MyRunner::new());
```

The system stores the runner in its configuration. The same runner is used for
the main system runtime and for runtimes created by new arbiters.

A custom runner controls how these runtimes are created, but it does not change
the task-spawning implementation selected at compile time. Functions such as
`ntex::rt::spawn()` use the backend chosen through Cargo features.

For example, when the `tokio` feature is enabled, `spawn()` uses the Tokio
backend. A custom runner cannot switch task spawning to the native runtime at
run time. The selected Cargo feature, custom runner, and runtime environment
must therefore be compatible.

## Blocking Work

Each arbiter is single-threaded. If one task blocks its thread, every other task
on that arbiter must wait. CPU-intensive work and blocking system calls should
not run directly inside asynchronous tasks.

Use [`spawn_blocking()`](https://docs.rs/ntex-rt/latest/ntex_rt/fn.spawn_blocking.html)
to move this work to the system's blocking thread pool:

```rust
let result = ntex::rt::spawn_blocking(|| {
    // Perform CPU-intensive or blocking work.
    expensive_computation()
})
.await?;
```

The asynchronous task can await the result without blocking other work on its
arbiter.

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

This mechanism is available only on Unix systems and does not guarantee that
every stalled arbiter will be detected.

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
