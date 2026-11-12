# Runtime

Each ntex runtime thread runs its own single-threaded event loop. Tasks stay
on the thread where they started. Your services and futures can use `Rc`,
`Cell`, `RefCell`, and other types that are not `Send` or `Sync`.

Your application can still use several CPU cores. A server normally starts
several worker threads, each with its own single-threaded runtime. State shared
between workers needs thread-safe types such as `Arc`, atomics, or locks.

For most applications, use `#[ntex::main]` as the entry point and
`#[ntex::test]` for async tests. Start tasks with `ntex::rt::spawn`. Use
`System` and `Arbiter` when you need explicit control over startup, shutdown,
or cross-thread coordination.

## Choosing a runtime backend

Choose a backend through Cargo features. [`DefaultRuntime`] selects both the
async runtime and its matching ntex network reactor:

* `tokio` uses Tokio. If a Tokio runtime handle is already entered on the
  current thread, ntex uses it. Otherwise, ntex creates a current-thread Tokio
  runtime. ntex tasks run inside a Tokio `LocalSet`.
* `compio` creates a Compio runtime on each runtime thread. Compio chooses the
  I/O driver for the platform and configuration.
* With neither feature, ntex uses its [native runtime](#native-runtime). On
  Linux it tries io_uring and falls back to polling; other Unix platforms use
  polling, and Windows uses IOCP.

Tokio takes precedence if you enable both `tokio` and `compio`. With the native
backend, `neon-polling` forces polling on Unix. The `neon-uring` feature
requires io_uring on Linux. Do not enable both native features.

For example:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio"] }
```

Tokio and Compio are useful when your application uses libraries from those
ecosystems. The native backend uses the platform reactor directly, without
running a second general-purpose runtime.

Let `#[ntex::main]` own the process entry point unless you need another setup.
`SystemRunner::block_on` blocks the current thread. Do not call it inside an
async Tokio task to nest one runtime inside another.

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

Neither the future nor its result needs to be `Send`. The future must be
`'static`, so use `async move` when it captures owned values. Calling `spawn()`
outside a running ntex runtime panics.

Await the returned [`JoinHandle`] when you need the task's output. If you no
longer need the work, call `cancel()`. Use `is_finished()` to check whether it
has finished without awaiting it.

The handle is **neither `Send` nor `Sync`**, even when the result is `Send`.
Keep it on the thread that created it. If the task panics or is cancelled,
awaiting the handle returns a `JoinError` instead of the task's output.

By default, dropping a `JoinHandle` detaches the task: it continues without
anyone waiting for its result. Call `cancel_on_drop()` on the handle to request
cancellation instead when it is dropped. The older `detach()` method also
detaches a task, but is deprecated. A detached task is not guaranteed to finish
before its arbiter or the whole system shuts down.

### Spawning on another thread

To run work on another runtime thread, get that arbiter's `Handle` and call
`spawn()` on it. Both the future and its output must be `Send + 'static`.

`Handle::spawn()` returns a [`RemoteJoinHandle<T>`][`RemoteJoinHandle`] rather
than a `JoinHandle<T>`. Like the local handle, it is neither `Send` nor `Sync`.
Keep it on the thread that called `spawn()`.

```rust
async fn example(handle: ntex_rt::Handle) {
    let task = handle.spawn(async { 42 });
    assert_eq!(task.await.unwrap(), 42);
}
```

The runtime thread owns the submitted work. On that thread, `Handle::spawn()`
spawns a local task. From another thread, it puts the future in a single
allocation and hands it to the runtime thread to poll and drop. The allocation
later holds the output too. All three backends behave this way.

Dropping a `RemoteJoinHandle` detaches the task. Call
`RemoteJoinHandle::cancel()` to cancel it on the runtime thread. The handle
returns `JoinError` if the runtime shuts down or the task is cancelled before
producing a result.

If the runtime rejects a submission, however, it never polls the future and
may drop it on the submitting thread instead.

The two handle types reflect these different needs. Keeping them separate
makes the common local case small: a native `JoinHandle` is a single pointer.
`RemoteJoinHandle` supports both paths, while same-thread submissions still
avoid the shared allocation.

Use `Handle::spawn_detached()` when you do not need a result. It returns `()`
and discards the output on the runtime thread. The output does not need to be
`Send`.

A submitted future can start non-`Send` work with `ntex::rt::spawn`. The
submitted future must stay `Send`, so it cannot hold that local `JoinHandle`
across an `.await`. Await the local result inside a local task instead.

[`JoinHandle`]: https://docs.ntex.rs/api/ntex/rt/struct.JoinHandle.html
[`RemoteJoinHandle`]: https://docs.ntex.rs/api/ntex/rt/struct.RemoteJoinHandle.html
[`ntex::rt::spawn`]: https://docs.ntex.rs/api/ntex/rt/fn.spawn.html

## Timers and timeouts

Use `ntex::time` rather than a backend-specific timer, so the code works with
all three backends:

```rust
use ntex::time::{Millis, sleep, timeout};

#[ntex::main]
async fn main() {
    sleep(Millis(10)).await;

    let result = timeout(Millis(100), async { "done" }).await;
    assert_eq!(result.unwrap(), "done");
}
```

ntex timers are for scheduling and timeouts, rather than precise measurement.
Their resolution is about 16 milliseconds. Even a zero-duration `sleep` or
`timeout` waits for at least one timer tick. Use `timeout_checked` if a zero
timeout should mean "no timeout".

See [Timers and Time Utilities](12-time.md) for the timer design and full API.

## System lifecycle

Use [`System`] to manage the runtime as a whole. It holds shared runtime
configuration and keeps track of arbiters. It also delivers signals and
provides the blocking thread pool.

`#[ntex::main]` builds a system for you. Build one yourself when you need
explicit control; this gives you a [`SystemRunner`]:

```rust
fn main() {
    let result = ntex::rt::System::build()
        .name("worker")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async { 10usize });

    assert_eq!(result, 10);
}
```

The lifetime of the root future determines when `block_on(future)` returns.
It starts the system, runs that future, and returns its output when the future
completes, as in the example above.

For a process that should keep running until explicitly stopped, use
`run(callback)` instead. It calls a synchronous startup callback inside the
running system, then keeps the event loop going until `System::stop()` or
`System::stop_with_code()` is called:

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

Use `run_until_stop()` to run without a startup callback. It otherwise behaves
like `run()`. Each of these methods consumes the runner. You cannot call
`block_on()` and then `run_until_stop()` on the same runner.

A non-zero stop code is returned as an I/O error.

Signal and panic handling are off by default. Enable signal handling to make
process signals available through `ntex::rt::signals`. Your application still
decides how to shut down. ntex servers listen for signals while they run and
handle their own shutdown.

Fatal signal handling is off by default as well. On Unix,
`Builder::fatal_signals(true)` installs process-wide `SIGSEGV` and `SIGABRT`
handlers that report these signals as `Signal::Panic` while signal handling is
enabled. The process still terminates.

[`System`]: https://docs.ntex.rs/api/ntex_rt/struct.System.html
[`SystemRunner`]: https://docs.ntex.rs/api/ntex_rt/struct.SystemRunner.html

## Arbiters and runtime threads

Use an [`Arbiter`] when you need another runtime thread with its own event
loop. Every system already has a primary arbiter. Call `Arbiter::new()` or
`Arbiter::with_name()` to start another OS thread with the same runtime
configuration. ntex server workers are arbiter threads too.

`Arbiter::current()` returns the arbiter for the current thread. It panics
outside an arbiter. For an arbiter created with `new()` or `with_name()`, call
`stop()` to request shutdown. Call `join()` to wait for its thread to exit.

The primary arbiter runs on the thread that started the system. It has no
thread handle, and its `join()` returns immediately. You cannot stop it on its
own; stop the `System` to end it.

Choose the shutdown scope deliberately. `Arbiter::stop()` stops one separately
created runtime thread; `System::stop()` stops every registered arbiter and
ends the system.

The same distinction applies to typed storage. Use `System::get_value()` for
a thread-safe value shared by the whole system. Use `Arbiter::get_value()` for
a cloneable value local to one runtime thread, such as a per-worker client or
cache that should not be shared.

[`Arbiter`]: https://docs.ntex.rs/api/ntex_rt/struct.Arbiter.html

## Blocking work

Blocking an arbiter thread stalls connection handling, timers, and every other
task on that thread. Move CPU-heavy work and blocking system calls to a
thread pool with [`spawn_blocking()`]:

```rust
#[ntex::main]
async fn main() {
    let total = ntex::rt::spawn_blocking(|| (0..1_000u64).sum::<u64>())
        .await
        .unwrap();

    assert_eq!(total, 499_500);
}
```

The closure and its result must be `Send`. Inside a system, the work runs on a
blocking pool that grows and shrinks as needed.

By default, the pool allows up to 256 threads. An idle thread exits after 60
seconds. Set the limit with `thread_pool_limit()` and the idle timeout with
`thread_pool_recv_timeout()`:

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

Outside a running system, `spawn_blocking()` runs the closure immediately on
the calling thread. It does **not** start a background thread. The returned
future is already complete. This fallback does not start background work
before the runtime is up.

Dropping the returned future stops queued work from starting. It cannot
interrupt a closure that is already running. Call `detach()` if the work should
go ahead even when nobody needs the result. Cancellation and panics in the
closure are reported as `BlockingError`.

[`spawn_blocking()`]: https://docs.ntex.rs/api/ntex_rt/fn.spawn_blocking.html

## Runtime diagnostics

If requests slow down, runtime diagnostics help you check whether arbiters
are responding and tasks are getting time to run. The system pings each
spawned arbiter every two seconds. `System::list_arbiter_pings()` returns the
ten most recent round-trip times. Set `ping_interval(0)` to turn pings off.

For a closer view of scheduling delays, each arbiter measures its latency
every `latency_interval()`, 100 milliseconds by default. `Arbiter::latency()`
returns a histogram of how late those measurements woke up.

A watchdog thread marks an arbiter as stalled if no measurement arrives within
the interval plus `ping_threshold()`. The threshold defaults to one second.
`Arbiter::latency()` also counts these stalls. Set `latency_interval(0)` to turn
off both measurement and stall detection.

To help locate a stall on the native (neon) runtime,
`Handle::current_task()` returns the spawn location of the task being polled.
The stall log includes this location. A stall with no running task happened
in the driver; tasks that ntex spawns itself report a location inside ntex.
On Tokio and Compio, `current_task()` returns `None`.

A busy task may also keep making itself runnable without waiting for input.
The native runtime counts these self-wakes in `Handle::self_wake_count()`.
A self-wake is a wake a task triggers while it is being polled.

If a task wakes itself 10,000 times in a row over at least one second without
waiting, the runtime logs a possible busy loop. It logs the warning once, with
the task's spawn location. Adjust the limits with
`RuntimeBuilder::self_wake_threshold()`. A count of zero turns the warning off.

On Linux and Windows, use `System::set_latency_callback()` to receive a
backtrace of a stalled arbiter. The callback also gets the task's spawn
location if the same task is still running after the capture. Without a
callback, the backtrace is logged.

On Linux, capturing a backtrace requires ntex signal handling and uses
`SIGUSR2`. If you embed ntex, do not use that signal for anything else. On
Windows, the watchdog briefly suspends the stalled thread and walks its stack.
This is supported on x86_64 and aarch64 only.

## Custom runners

Use a custom runner when your application provides both an executor and a
compatible network reactor. Otherwise, use `DefaultRuntime`.

`System::build().build(...)` accepts any implementation of `ntex_rt::Runner`.
ntex networking expects a compatible reactor on every runtime thread. Your
runner must set up everything the chosen backend relies on; replacing the
executor alone is not enough.

## Runtime attributes

`#[ntex::main]` turns an async function into a synchronous entry point. It
builds a `System` and drives the function's future:

```rust
#[ntex::main]
async fn main() {
    // Start the application.
}
```

It accepts these options:

* `name = "..."` sets the system name;
* `signals = true` or `false`;
* `fatal_signals = true` or `false`;
* `panic_handling = true` or `false`;
* `ping_interval = N` in milliseconds, where zero turns off arbiter pings;
* `rt = Type` selects a custom runtime runner.

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

The async function can return a value such as `Result<(), E>`, and the
generated synchronous function returns it.

Use `#[ntex::test]` to run an async test in a fresh system named after the test.
The attribute marks it as a test system and turns off signal, fatal signal, and
panic handling.
It also sets up ntex test logging:

```rust
#[ntex::test]
async fn runtime_test() {
    let value = ntex::rt::spawn(async { 10usize }).await.unwrap();
    assert_eq!(value, 10);
}
```

The `no-test-logging` feature skips the logging setup.

## Native runtime

The native runtime, called neon, pairs a task runner with a platform-specific
I/O reactor. Its design takes advantage of the same thread locality that lets
your services use `Rc`: waking a task on its own thread needs no
synchronization. The following sections explain that design; you do not need
these details to use ntex.

Rust's standard `Waker` must be `Send + Sync`. Any thread can hold a clone and
call `wake()`. Tokio and async-task meet that requirement by synchronizing
every waker operation. Their reference counts and task state are atomics, so
each clone, wake, and drop pays for synchronization, even on the task's own
thread.

The native runtime separates local and remote access. A waker first detects
which thread is using it. On the task's owner thread, state and reference
counts are plain values wrapped in `Cell`. Cloning, waking, and dropping a
waker need no atomic operations, and a woken task goes straight onto the local
ready queue.

From another thread, the waker never touches the task directly. It pushes a
command onto a Crossbeam queue for the owner thread to apply on its next turn.
This makes remote wakes slower than in Tokio or async-task, but local wakes
cheaper. Most wakes in an ntex application are local because a connection's
I/O, timers, and services live on one thread.

### Owner-thread wake handles

A timer or I/O waiter often needs to keep a wake handle for later. For these
local wakes, [`ntex_rt::LocalWaker`] gives ntex code an owned handle that stays
on its owner thread. Timers, I/O waiters, and notification slots store these
handles internally. Despite the similar name, `ntex::task::LocalWaker` is a
different type: it is a notification slot, not the handle stored in one.

Creating a `LocalWaker` from a borrowed standard waker checks its kind, owner
thread, and runtime. It then takes a local reference to the task. After that,
cloning, waking, and dropping the handle need no further thread checks.

Keep the handle to benefit from these local operations. Acquiring a new one
and converting it back every time has a cost. `into_waker()` consumes the
handle and returns a thread-safe standard waker. For native tasks, it takes
the standard-waker reference before releasing the local one. The local handle
itself cannot cross threads.

On 64-bit targets, the handle, `Option<LocalWaker>`, and the notification slot
are each 16 bytes. Neither native nor fallback handles allocate a wrapper.
A native task uses a single allocation for routing metadata, its pinned
execution core, and storage for the future and result.

There is no separate wake-token allocation or per-task lookup table. The
remote command queue does allocate segments. Arbitrary waker callbacks,
timers, and I/O storage may allocate as they grow.

These changes speed up a hot path without guaranteeing faster scheduling or
request handling. Registering the same waker again still requires a kind
check. Tokio, Compio, and wrapped or custom wakers keep their original
callbacks and may pay a small extra dispatch cost.

### Remote wakes

Remote wakes must keep task storage alive without changing its local state.
They do this by sending commands to the owner thread through a Crossbeam
FIFO, a first-in, first-out queue. Only the owner changes a task's reference
count, even after completion. A standard native waker used on another thread
publishes reference and wake commands without touching the count.

On 64-bit targets, each command is one pointer-sized word. Task alignment
leaves three low bits free to store the operation. The encoding preserves
pointer provenance, and the tag is removed before the task is accessed. Other
targets store the pointer and operation separately, without increasing task
alignment.

To avoid queue work for tasks that stay local, each task has an atomic flag
that records whether a remote waker command has ever been published for it.
The flag is set just before the first such command and never cleared. Tasks
without the flag release their references directly, including the last one,
without looking at the FIFO.

Submitting a task from another thread does not set the flag. Its initial
reference passes to the owner before the task can hand out a waker.

Once the flag is set, the owner's final release waits behind acquisitions
already queued. While commands are pending, the owner's other releases go
through the queue too. Otherwise, a release could overtake an earlier queued
release and invalidate a later wake or acquisition.

When a remotely cloned waker returns to the owner, the flag store is
synchronized. Until then, the waker it was cloned from keeps the allocation
alive. Processing a queued command also synchronizes with the flag store.
The flag is shared metadata only; remote threads never update the reference
count.

### Task completion

A finished task and a freed task allocation are not the same thing.
Completing a task drops its future, but wake and join handles may keep its
storage allocated. That leftover storage is not a running future.

The storage remains while any native standard waker, local handle, or join
handle points to it. A join handle also owns the result until it is taken.
Dropping the join handle frees the result, whether or not wake handles remain.

The list of unfinished tasks does not own them, so it never keeps an orphaned
future alive.

### Routing remote commands

A standard waker uses a global registry to find where to send commands. The
registry maps runtime IDs to runtimes, and IDs are never reused.

Looking up every remote wake in the registry would add synchronization.
Instead, each remote thread caches owned references to runtime endpoints it
has used. A cache hit skips the registry lookup and per-operation reference
counting on the queue. Caching a new endpoint also removes closed ones;
accessing a closed cached endpoint removes it too.

Remote waker commands and unpolled submissions reserve admission before they
publish. Shutdown therefore waits for accepted work, even when the publisher
bypassed the registry. A closed cached endpoint rejects new work.

Cleanup must not call back into the runtime while these shared structures
are in use. Notifications therefore run only after admission, registry locks,
and cache borrows are released. Removed endpoints are also dropped outside
locks or borrows, before the new operation is admitted, so driver destructors
can safely re-enter. Wakers used while thread-locals are being destroyed fall
back to the registry.

The owner thread caches the last runtime ID it checked and whether that
endpoint is open. A hit also proves that the current thread owns the runtime,
saving a separate thread-ID lookup. A miss checks ownership before consulting
the registry.

Shutdown records the closed state in this cache before running callbacks.
Because IDs are never reused, that entry stays correct. This cache holds no
queue reference. Switching between runtimes on one thread goes back to the
registry.

Cross-thread task submissions use this routing too. Unpolled `Send`
submissions travel through the same FIFO as remote wakes. Once a task runs,
its state, references, join results, and awaiters are all local to the owner.
Local wakes use an intrusive ready queue, whose links live in the task rather
than separate queue entries. Native join awaiters hold `LocalWaker`s that can
still be released after they unregister.

Each task stores a raw pointer to its queue, rather than a strong or weak
reference. Only the owner thread uses it. Unfinished tasks use it while their
runtime or runner keeps the queue alive. Completed tasks need a live endpoint
before borrowing it.

Deferred releases on a busy owner publish without taking a temporary strong
queue reference. Releases that may send a notification or run shutdown cleanup
still take one. Cleanup that can run callbacks resolves the runtime ID to a
strong queue reference outside the registry lock.

### Limits and memory layout

Keeping routing information in each task makes its size important. The
native runtime uses 16-bit runtime IDs and owner-thread indexes to keep the
task core at 80 bytes on 64-bit targets.

A process can create at most 65,535 native runtimes. At most 65,535 threads can
own one. Neither IDs nor indexes are reused, and the process aborts if either
runs out. Threads that only wake tasks or spawn through a `Handle` do not use
an index.

Fields used by wakes, runs, and releases sit in the core's first 64 bytes.
Task-list links come last; they are used only on spawn, completion, and
shutdown. Task allocations are not cache-line aligned. This layout makes
single-cache-line access more likely without guaranteeing it.

After running local tasks, the scheduler checks whether the remote FIFO is
empty before trying to pop from it. This avoids the full memory fence that an
empty pop costs. The scheduler rechecks readiness afterward. A command
published in the meantime either keeps the runtime polling or wakes the driver.

A non-empty FIFO is drained in order, within the scheduler's per-turn budget.

### Shutdown

Shutdown has to account for commands already on their way to the owner.
It stops new work and drops live futures on their owner thread, first closing
admission and disabling the runtime's wake endpoint, then draining accepted
commands.

The closed registry entry remains available for owner cleanup, including panic
cleanup, until draining finishes. Removing it only afterward prevents owner
releases from overtaking pending commands. After removal, standard-waker
operations are ignored. Cloning such a waker produces an inert one.

**Standard wakers still alive at shutdown permanently keep their task
allocations**, even after they are dropped. The memory remains, but execution
does not continue. Task allocations do not keep the driver or the queue's
backing memory alive.

Separately, a remote thread's endpoint cache can keep closed queues and their
drivers alive. A closed entry is released when the thread next accesses it,
caches a new endpoint, or exits. Owner-local handles can still be released
after shutdown without touching the possibly dangling queue pointer.

If you need to reclaim task memory, drop standard wakers and let the owner
process their queued releases before shutdown.

[`ntex_rt::LocalWaker`]: https://docs.ntex.rs/api/ntex_rt/struct.LocalWaker.html
