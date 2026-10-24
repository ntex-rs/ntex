# Runtime

ntex uses a single-threaded execution model on each runtime thread. Tasks stay
on the thread where they were started, so services and futures can use `Rc`,
`Cell`, `RefCell`, and other types that are not `Send` or `Sync`.

That does not limit an application to one CPU core. A server normally starts
several worker threads, each with its own single-threaded runtime. State shared
between workers must still use thread-safe types such as `Arc`, atomics, or
locks.

Most applications only need `#[ntex::main]`, `#[ntex::test]`, and
`ntex::rt::spawn`. The lower-level `System` and `Arbiter` APIs are useful for
explicit startup, shutdown, and cross-thread coordination.

## Choosing a runtime backend

[`DefaultRuntime`] selects the async runtime and matching ntex network reactor
together. Cargo features choose the backend:

* `tokio` uses Tokio. When a Tokio runtime handle is entered on the current
  thread, the adapter uses it; otherwise it creates a current-thread Tokio
  runtime. ntex tasks run in a Tokio `LocalSet`.
* `compio` creates a Compio runtime on each runtime thread. Compio selects the
  I/O driver for the host platform and configuration.
* without either feature, ntex uses its native runtime. On Linux it tries
  io_uring and falls back to polling; other Unix platforms use polling, and
  Windows uses IOCP.

If both `tokio` and `compio` are enabled, Tokio takes precedence. With the
native backend, `neon-polling` forces polling on Unix and `neon-uring` requires
io_uring on Linux. Do not enable both explicit native reactor features.

For example:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio"] }
```

The Tokio and Compio backends are convenient when an application also uses
libraries from those ecosystems. The native backend avoids an additional
general-purpose runtime and lets ntex choose its platform reactor directly.

`SystemRunner::block_on` blocks the current thread. Do not call it from inside
an async Tokio task to try to nest one runtime inside another. Usually the
cleanest boundary is to let `#[ntex::main]` own the process entry point.

[`DefaultRuntime`]: https://docs.ntex.rs/api/ntex/rt/struct.DefaultRuntime.html

## Spawning tasks

[`ntex::rt::spawn`] starts a future on the current runtime thread:

```rust
#[ntex::main]
async fn main() {
    let task = ntex::rt::spawn(async {
        // `Rc`, `RefCell`, and other `!Send` values may be used here.
        10usize
    });

    assert_eq!(task.await.unwrap(), 10);
}
```

The future and its result do not need to implement `Send`, but the future must
be `'static`. Use `async move` when it captures owned values. `spawn()` panics
when called outside an active ntex runtime.

The returned [`JoinHandle`] can be awaited, canceled with `cancel()`, detached
with `detach()`, or queried with `is_finished()`. A task panic or cancellation
is reported as `JoinError`, not as the task's normal output.

Dropping a local join handle detaches the task; it does not cancel it. Keep and
await the handle when the result matters. A detached task is not guaranteed to
finish before its arbiter or the whole system shuts down.

To submit work to another runtime thread, obtain an arbiter's handle and call
`spawn()` on it. The future and its output must both be `Send + 'static`
because they cross thread boundaries. Once the future is running on that
arbiter, it can start more non-`Send` work with `ntex::rt::spawn`.

Do not use a remote join handle as a portable abort mechanism. With the Tokio
and Compio backends, canceling work sent through another arbiter abandons the
result but does not stop the remote task. Use an explicit cancellation signal
when cross-thread work must be stoppable.

[`JoinHandle`]: https://docs.ntex.rs/api/ntex/rt/struct.JoinHandle.html
[`ntex::rt::spawn`]: https://docs.ntex.rs/api/ntex/rt/fn.spawn.html

## Owner-thread wake handles

[`ntex_rt::LocalWaker`] is an owned, thread-confined wake handle, not the
notification slot named `ntex::task::LocalWaker`. Native ntex task wakers
acquired on their owner thread use local operations without repeating the
thread-identity check on every clone, wake, or drop. Timers, I/O waiters, and
notification slots retain these handles internally.

Acquisition from a borrowed standard waker still checks its representation
and clones wake ownership. Retaining the handle amortizes that work;
repeatedly acquiring and converting it is not a free optimization.
`into_waker()` transfers ownership back to a thread-safe standard waker
without cloning. Local handles cannot themselves cross threads.

On 64-bit targets the handle, its `Option`, and the notification slot each
occupy 16 bytes. Neither native nor fallback handles add a wrapper allocation,
and the native task header does not grow. Arbitrary waker callbacks may still
allocate, as may timer or I/O storage when it first grows.

This is a hot-path optimization, not a guarantee of faster task scheduling or
request handling. Same-waker registration still has representation-checking
overhead. Tokio, Compio, and wrapped or custom wakers retain their original
callbacks and may incur small additional dispatch costs. Completed native
tasks still use shared atomic reference accounting, and mixed remote wakes
still contend on shared task state.

[`ntex_rt::LocalWaker`]: https://docs.ntex.rs/api/ntex_rt/struct.LocalWaker.html

## Timers and timeouts

Use `ntex::time` instead of a backend-specific timer so the code works with
all three runtime backends:

```rust
use ntex::time::{Millis, sleep, timeout};

#[ntex::main]
async fn main() {
    sleep(Millis(10)).await;

    let result = timeout(Millis(100), async { "done" }).await;
    assert_eq!(result.unwrap(), "done");
}
```

ntex timers are intended for scheduling and timeouts, not high-resolution
measurement. Their granularity is roughly 16 milliseconds. A zero-duration
`sleep` or `timeout` still waits for at least one timer tick.
`timeout_checked` is the variant that treats a zero timeout as disabled.

## System lifecycle

[`System`] is the top-level runtime context. It owns the shared runtime
configuration, tracks arbiters, manages signal delivery, and provides the
blocking thread pool. `#[ntex::main]` builds a system automatically.

Building one manually returns a [`SystemRunner`]:

```rust
fn main() {
    let result = ntex::rt::System::build()
        .name("worker")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async { 10usize });

    assert_eq!(result, 10);
}
```

`SystemRunner` has two common ownership models:

* `block_on(future)` starts a system, drives one root future, and returns its
  output when that future completes;
* `run(callback)` invokes a synchronous startup callback inside the running
  system, then keeps the event loop alive until `System::stop()` or
  `System::stop_with_code()` is called.

For a process driven by an explicit stop request:

```rust,no_run
use std::io;
use ntex::rt::{self, DefaultRuntime, System};
use ntex::time::{Millis, sleep};

fn main() -> io::Result<()> {
    System::build()
        .name("my-app")
        .build(DefaultRuntime)
        .run(|| {
            rt::spawn(async {
                sleep(Millis(10)).await;
                System::current().stop();
            });
            Ok(())
        })
}
```

`run_until_stop()` is `run()` without a startup callback. All these methods
consume the runner, so choose one model rather than calling `block_on()` and
then `run_until_stop()` on the same value. A non-zero stop code is returned as
an I/O error.

Signal and panic handling are disabled by default. Enabling signal handling
makes process signals available through `ntex::rt::signals`; it does not by
itself define the application's shutdown policy. ntex servers listen for
process signals while they are running and coordinate their own shutdown.

[`System`]: https://docs.ntex.rs/api/ntex_rt/struct.System.html
[`SystemRunner`]: https://docs.ntex.rs/api/ntex_rt/struct.SystemRunner.html

## Arbiters and runtime threads

An [`Arbiter`] represents one runtime event-loop thread. Every system has a
primary arbiter. `Arbiter::new()` or `Arbiter::with_name()` starts another OS
thread using the same system runtime configuration. ntex server workers are
also arbiter threads.

`Arbiter::current()` returns the arbiter for the current thread and panics when
called outside one. For an arbiter created with `new()` or `with_name()`,
`stop()` requests shutdown and `join()` waits for its thread to exit.

The primary arbiter runs on the thread that started the system. It cannot be
stopped independently, has no owned thread handle, and `join()` returns
immediately. Stop the `System` to end the primary arbiter.

The two levels have different shutdown scopes:

* `Arbiter::stop()` stops one separately created runtime thread;
* `System::stop()` stops every registered arbiter and ends the system.

The runtime also offers two typed storage scopes. `System::get_value()` stores
a thread-safe value shared by the complete system, while
`Arbiter::get_value()` stores a cloneable value local to one runtime thread.
The latter is useful for per-worker clients or caches that should not be
shared.

[`Arbiter`]: https://docs.ntex.rs/api/ntex_rt/struct.Arbiter.html

## Blocking work

Blocking an arbiter thread pauses connection handling, timers, and every other
task on that thread. Move CPU-heavy work and blocking system calls to
[`spawn_blocking()`]:

```rust
#[ntex::main]
async fn main() {
    let total = ntex::rt::spawn_blocking(|| (0..1_000u64).sum::<u64>())
        .await
        .unwrap();

    assert_eq!(total, 499_500);
}
```

The closure and its result must implement `Send`. Inside a system, the work
runs on a dynamically sized blocking pool. The pool allows up to 256 workers
by default, and an idle worker exits after 60 seconds. Configure those values
with `thread_pool_limit()` and `thread_pool_recv_timeout()`:

```rust,no_run
use std::time::Duration;

fn main() {
    ntex::rt::System::build()
        .thread_pool_limit(32)
        .thread_pool_recv_timeout(Duration::from_secs(30))
        .build(ntex::rt::DefaultRuntime)
        .block_on(async {});
}
```

If `spawn_blocking()` is called outside a running system, it does **not**
create a background worker. The closure runs immediately on the calling thread
and the returned future is already ready. Treat that as a fallback, not as a
way to start asynchronous work before the runtime.

Dropping the returned future prevents queued work from starting, but cannot
interrupt a closure that is already running. Use `detach()` when queued work
should continue even if nobody needs its result. Cancellation and closure
panics are reported as `BlockingError`.

[`spawn_blocking()`]: https://docs.ntex.rs/api/ntex_rt/fn.spawn_blocking.html

## Runtime diagnostics

The system pings spawned arbiters every two seconds and keeps their ten most
recent round-trip records. `System::list_arbiter_pings()` exposes those
records. Set `ping_interval(0)` to disable pings.

Every arbiter also samples its scheduling latency every `latency_interval()`,
100 milliseconds by default. `Arbiter::latency()` returns a histogram of how
late the samples woke up. A watchdog thread marks an arbiter as stalled when it
records no sample for longer than the interval plus `ping_threshold()`, which
defaults to one second, and counts stalls in `Arbiter::latency()`. Set
`latency_interval(0)` to disable sampling and stall detection.

On the default neon runtime, `Handle::current_task()` returns the spawn
location of the task the runtime is polling, and the stall log names it. A
stall without a running task happens in the driver. Tasks spawned by ntex
itself report the location inside ntex. The tokio and compio runtimes return
`None`.

The neon runtime also counts self-wakes, wakes a task triggers while it is
being polled, in `Handle::self_wake_count()`. A task that wakes itself 10000
times in a row over at least one second without waiting for anything is logged
once with its spawn location as a possible busy loop. Change the limits with
`RuntimeBuilder::self_wake_threshold()`; a count of zero disables the warning.

On Linux and Windows, `System::set_latency_callback()` can receive a backtrace
when an arbiter stalls, together with the spawn location of the running task
when it is still the same task after the capture; without a callback the
backtrace is logged.
On Linux, backtrace capture requires ntex signal handling and uses `SIGUSR2`;
an embedding application should not reserve that signal for another purpose.
On Windows, the watchdog briefly suspends the stalled arbiter thread and walks
its stack (x86_64 and aarch64 only).

## Custom runners

`System::build().build(...)` accepts any implementation of `ntex_rt::Runner`.
This is an advanced integration point, not merely an executor switch: ntex
networking also expects a compatible reactor on every runtime thread. A custom
runner must establish everything expected by the selected backend.

Use `DefaultRuntime` unless the application deliberately provides both sides
of that integration.

## Runtime attributes

`#[ntex::main]` turns an async function into a synchronous entry point, builds
a `System`, and drives the function's future:

```rust
#[ntex::main]
async fn main() {
    // Start the application.
}
```

Supported options are:

* `name = "..."` for the system name;
* `signals = true` or `false`;
* `panic_handling = true` or `false`;
* `ping_interval = N` in milliseconds, where zero disables arbiter pings;
* `rt = Type` to select a custom runtime runner.

For example:

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

The async function may return a value such as `Result<(), E>`; the generated
synchronous function returns the root future's output.

`#[ntex::test]` creates a fresh system named after the test, marks it as a
testing system, disables signal and panic handling, and initializes ntex test
logging:

```rust
#[ntex::test]
async fn runtime_test() {
    let value = ntex::rt::spawn(async { 10usize }).await.unwrap();
    assert_eq!(value, 10);
}
```

Test logging initialization is skipped when the `no-test-logging` feature is
enabled.

The useful boundaries to remember are:

* local tasks may be non-`Send`, but cross-arbiter work must be `Send`;
* dropping a join handle does not stop its task;
* blocking closures belong in `spawn_blocking`;
* ntex timers keep code independent of the selected backend;
* stop a separately created `Arbiter` for one runtime thread and the `System`
  for the whole runtime;
* use `DefaultRuntime` unless the application also provides the matching ntex
  network reactor.
