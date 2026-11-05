# Runtime

Each ntex runtime thread runs its own single-threaded event loop. A task stays
on the thread where it was started, so services and futures can freely use
`Rc`, `Cell`, `RefCell`, and other types that are not `Send` or `Sync`.

This does not limit an application to one CPU core. A server normally starts
several worker threads, each with its own single-threaded runtime. State shared
between workers still needs thread-safe types such as `Arc`, atomics, or locks.

Most applications only need `#[ntex::main]`, `#[ntex::test]`, and
`ntex::rt::spawn`. The lower-level `System` and `Arbiter` APIs are there for
explicit startup, shutdown, and cross-thread coordination.

## Choosing a runtime backend

[`DefaultRuntime`] picks the async runtime and the matching ntex network
reactor together. Cargo features decide which backend it uses:

* `tokio` uses Tokio. If a Tokio runtime handle is already entered on the
  current thread, ntex uses it; otherwise it creates a current-thread Tokio
  runtime. ntex tasks run inside a Tokio `LocalSet`.
* `compio` creates a Compio runtime on each runtime thread. Compio chooses the
  I/O driver for the platform and configuration.
* With neither feature, ntex uses its [native runtime](#native-runtime). On
  Linux it tries io_uring and falls back to polling; other Unix platforms use
  polling, and Windows uses IOCP.

If both `tokio` and `compio` are enabled, Tokio wins. With the native backend,
`neon-polling` forces polling on Unix and `neon-uring` requires io_uring on
Linux. Do not enable both.

For example:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio"] }
```

Tokio and Compio are convenient when an application also uses libraries from
those ecosystems. The native backend avoids running a second general-purpose
runtime and lets ntex use the platform reactor directly.

`SystemRunner::block_on` blocks the current thread. Do not call it from inside
an async Tokio task in an attempt to nest one runtime inside another. The
cleanest setup is usually to let `#[ntex::main]` own the process entry point.

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

Neither the future nor its result needs to be `Send`, but the future must be
`'static`, so use `async move` when it captures owned values. `spawn()` panics
if it is called outside a running ntex runtime.

The returned [`JoinHandle`] can be awaited, canceled with `cancel()`, detached
with `detach()`, or checked with `is_finished()`. It is **neither `Send` nor
`Sync`**, even when the result is `Send`, and must stay on the thread that
created it. If the task panics or is canceled, awaiting the handle returns a
`JoinError` instead of the task's output.

Dropping a `JoinHandle` detaches the task; it does not cancel it. Keep the
handle and await it when you need the result. A detached task is not
guaranteed to finish before its arbiter or the whole system shuts down.

### Spawning on another thread

To run work on another runtime thread, get that arbiter's `Handle` and call
`spawn()` on it. Both the future and its output must be `Send + 'static`.
**`Handle::spawn()` returns a `RemoteJoinHandle<T>`**, not a `JoinHandle<T>`.
Like the local handle, it is neither `Send` nor `Sync` and stays on the thread
that called `spawn()`.

```rust
async fn example(handle: ntex_rt::Handle) {
    let task = handle.spawn(async { 42 });
    assert_eq!(task.await.unwrap(), 42);
}
```

Called on the runtime's own thread, `Handle::spawn()` simply spawns a local
task. From any other thread, the future is placed in a single allocation,
which later also holds its output, and handed to the runtime thread, where it
is polled and dropped. This works the same way on the native, Tokio, and
Compio backends.

Dropping a `RemoteJoinHandle` detaches the task, and
`RemoteJoinHandle::cancel()` cancels it on the runtime thread. If the runtime
shuts down or the task is canceled before it produces a result, the handle
returns `JoinError`. A rejected submission is never polled, and its future may
be dropped on the submitting thread.

The two handle types are separate so the common local case stays small: a
native `JoinHandle` is a single pointer. `RemoteJoinHandle` supports both
paths, so a same-thread submission still avoids the shared allocation.

For fire-and-forget work, use `Handle::spawn_detached()`. It returns `()`, and
because the output is discarded on the runtime thread, the output does not need
to be `Send`.

A submitted future can start non-`Send` work with `ntex::rt::spawn`. It cannot
hold that local `JoinHandle` across an `.await`, though, because the submitted
future must stay `Send`. To await the local result, do it inside a local task.

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

ntex timers are meant for scheduling and timeouts, not precise measurement.
Their resolution is about 16 milliseconds, and even a zero-duration `sleep` or
`timeout` waits for at least one timer tick. Use `timeout_checked` if a zero
timeout should mean "no timeout". [Timers and Time Utilities](12-time.md)
covers the timer design and the full API.

## System lifecycle

[`System`] is the top-level runtime context. It holds the shared runtime
configuration, keeps track of arbiters, delivers signals, and provides the
blocking thread pool. `#[ntex::main]` builds a system for you.

Building one by hand gives you a [`SystemRunner`]:

```rust
fn main() {
    let result = ntex::rt::System::build()
        .name("worker")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async { 10usize });

    assert_eq!(result, 10);
}
```

A `SystemRunner` can be driven in two ways:

* `block_on(future)` starts the system, runs one root future, and returns its
  output when the future completes;
* `run(callback)` calls a synchronous startup callback inside the running
  system, then keeps the event loop going until `System::stop()` or
  `System::stop_with_code()` is called.

For a process that runs until something explicitly stops it:

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

`run_until_stop()` is `run()` without a startup callback. Each of these methods
consumes the runner, so pick one; you cannot call `block_on()` and then
`run_until_stop()` on the same runner. A non-zero stop code is returned as an
I/O error.

Signal and panic handling are off by default. Turning on signal handling makes
process signals available through `ntex::rt::signals`, but it does not decide
how the application shuts down. ntex servers listen for signals while they run
and handle their own shutdown.

[`System`]: https://docs.ntex.rs/api/ntex_rt/struct.System.html
[`SystemRunner`]: https://docs.ntex.rs/api/ntex_rt/struct.SystemRunner.html

## Arbiters and runtime threads

An [`Arbiter`] is one runtime thread with its own event loop. Every system has
a primary arbiter. `Arbiter::new()` and `Arbiter::with_name()` start another
OS thread with the same runtime configuration. ntex server workers are arbiter
threads too.

`Arbiter::current()` returns the arbiter for the current thread and panics
outside one. For an arbiter created with `new()` or `with_name()`, `stop()`
asks it to shut down and `join()` waits for its thread to exit.

The primary arbiter runs on the thread that started the system. It cannot be
stopped on its own, has no thread handle, and its `join()` returns
immediately. Stop the `System` to end it.

The two levels shut down different things:

* `Arbiter::stop()` stops one separately created runtime thread;
* `System::stop()` stops every registered arbiter and ends the system.

There are also two scopes for typed storage. `System::get_value()` stores a
thread-safe value shared by the whole system, while `Arbiter::get_value()`
stores a cloneable value local to one runtime thread. The latter is handy for
per-worker clients or caches that should not be shared.

[`Arbiter`]: https://docs.ntex.rs/api/ntex_rt/struct.Arbiter.html

## Blocking work

Blocking an arbiter thread stalls connection handling, timers, and every other
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

The closure and its result must be `Send`. Inside a system, the work runs on a
blocking pool that grows and shrinks as needed. By default the pool allows up
to 256 threads, and an idle thread exits after 60 seconds. Change these with
`thread_pool_limit()` and `thread_pool_recv_timeout()`:

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

Outside a running system, `spawn_blocking()` does **not** start a background
thread. The closure runs immediately on the calling thread, and the returned
future is already complete. Treat this as a fallback, not as a way to start
work before the runtime is up.

Dropping the returned future stops queued work from starting, but it cannot
interrupt a closure that is already running. Call `detach()` if the work should
go ahead even when nobody needs the result. Cancellation and panics in the
closure are reported as `BlockingError`.

[`spawn_blocking()`]: https://docs.ntex.rs/api/ntex_rt/fn.spawn_blocking.html

## Runtime diagnostics

The system pings each spawned arbiter every two seconds and keeps the ten most
recent round-trip times, which `System::list_arbiter_pings()` returns. Set
`ping_interval(0)` to turn pings off.

Each arbiter also measures its scheduling latency every `latency_interval()`,
100 milliseconds by default. `Arbiter::latency()` returns a histogram of how
late those measurements woke up. A watchdog thread marks an arbiter as stalled
when it records no measurement for longer than the interval plus
`ping_threshold()` (one second by default), and `Arbiter::latency()` counts
the stalls. Set `latency_interval(0)` to turn off both measurement and stall
detection.

On the native (neon) runtime, `Handle::current_task()` returns the spawn
location of the task being polled, and the stall log includes it. A stall with
no running task happened in the driver. Tasks that ntex spawns itself report a
location inside ntex. On Tokio and Compio, `current_task()` returns `None`.

The native runtime also counts self-wakes, that is, wakes a task triggers while
it is being polled, in `Handle::self_wake_count()`. If a task wakes itself
10,000 times in a row over at least one second without waiting for anything,
it is logged once, with its spawn location, as a possible busy loop. Adjust the
limits with `RuntimeBuilder::self_wake_threshold()`; a count of zero turns the
warning off.

On Linux and Windows, `System::set_latency_callback()` can receive a backtrace
of a stalled arbiter. If the same task is still running after the capture, the
callback also gets its spawn location. Without a callback, the backtrace is
logged. On Linux, capturing a backtrace requires ntex signal handling and uses
`SIGUSR2`, so an embedding application should not use that signal for anything
else. On Windows, the watchdog briefly suspends the stalled thread and walks
its stack (x86_64 and aarch64 only).

## Custom runners

`System::build().build(...)` accepts any implementation of `ntex_rt::Runner`.
This is an advanced integration point, not just a way to swap executors: ntex
networking also expects a compatible reactor on every runtime thread, and a
custom runner must set up everything the chosen backend relies on.

Stick with `DefaultRuntime` unless the application deliberately provides both
halves of that integration.

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

`#[ntex::test]` creates a fresh system named after the test, marks it as a
test system, turns off signal and panic handling, and sets up ntex test
logging:

```rust
#[ntex::test]
async fn runtime_test() {
    let value = ntex::rt::spawn(async { 10usize }).await.unwrap();
    assert_eq!(value, 10);
}
```

The `no-test-logging` feature skips the logging setup.

The main points to remember:

* local tasks can be non-`Send`, but work sent to another arbiter must be
  `Send`;
* dropping a join handle does not stop its task;
* blocking closures belong in `spawn_blocking`;
* ntex timers keep code independent of the backend;
* stop an `Arbiter` you created to end one runtime thread, and the `System` to
  end the whole runtime;
* use `DefaultRuntime` unless the application also provides the matching ntex
  network reactor.

## Native runtime

The native runtime, called neon (hence the `neon-polling` and `neon-uring`
features), has two parts: a task runner that polls futures, and a reactor that
performs I/O. The task runner is the same on every platform. The reactor
depends on the platform: io_uring or polling on Linux, polling on other Unix
systems, and IOCP on Windows.

Like the rest of ntex, the runtime is single-threaded and shares nothing. Each
runtime thread runs its own tasks, and a task never moves to another thread.

What sets it apart from Tokio or async-task is how tasks are woken. Rust's
standard `Waker` must be `Send + Sync`, because any thread can hold a clone
and call `wake()`. Tokio and async-task meet that requirement by synchronizing
every waker operation: reference counts and task state are atomics, so each
clone, wake, and drop pays for synchronization, even on the task's own thread.

The native runtime separates local and remote access instead. A waker detects
whether it is used on the thread that owns the task or on another thread:

* **On the owner thread**, task state and reference counts are plain values
  wrapped in `Cell`. Cloning, waking, and dropping a waker need no atomic
  operations, and a woken task goes straight onto the local ready queue.
* **From another thread**, the waker never touches the task directly. It
  pushes a command onto a Crossbeam queue, and the owner thread applies it on
  its next turn.

This is a trade-off. Local wakes are cheaper than in Tokio or async-task, and
most wakes in an ntex application are local, because a connection's I/O,
timers, and services all live on one thread. Remote wakes are slower than in
Tokio or async-task: each one goes through the queue, and the owner thread
finishes the work later.

The rest of this section covers the mechanics in detail. You do not need it to
use ntex.

### Owner-thread wake handles

[`ntex_rt::LocalWaker`] is how ntex code takes advantage of local wakes. It is
an owned wake handle that stays on its owner thread. Do not confuse it with
`ntex::task::LocalWaker`, which is a notification slot. Timers, I/O waiters,
and notification slots store these handles internally.

Creating a `LocalWaker` from a borrowed standard waker checks what kind of
waker it is, which thread owns the task, and which runtime it belongs to, and
then takes a local reference to the task. After that, cloning, waking, and
dropping the handle are local operations with no further thread checks. The
savings come from keeping the handle: acquiring a new one and converting it
back every time is not free. `into_waker()` consumes the handle and returns a
thread-safe standard waker; for native tasks it takes the standard-waker
reference before releasing the local one. The handle itself cannot cross
threads.

On 64-bit targets, the handle, `Option<LocalWaker>`, and the notification slot
are each 16 bytes, and neither native nor fallback handles allocate a wrapper.
A native task is a single allocation that holds its routing metadata, its
pinned execution core, and the storage for its future and result. There is no
separate wake-token allocation and no per-task lookup table. The remote
command queue does allocate segments, and arbitrary waker callbacks, timers,
and I/O storage may allocate as they grow.

All of this speeds up a hot path; it does not guarantee faster scheduling or
request handling. Registering the same waker again still pays for checking its
kind. Tokio, Compio, and wrapped or custom wakers keep their original
callbacks and may pay a small extra dispatch cost.

### Remote wakes

Only the owner thread changes a task's reference count, even after the task
completes. A standard native waker used on another thread therefore never
touches the count. Instead, it publishes reference and wake commands to the
owner through a Crossbeam FIFO. On 64-bit targets each command is one
pointer-sized word, with the operation stored in the three low bits that task
alignment leaves free. The encoding preserves pointer provenance, and the tag
is removed before the task is accessed. Other targets store the pointer and the
operation separately, without increasing task alignment.

Each task also has a sticky atomic flag. It is set just before the first
remote waker command for the task is published, and it is never cleared. Tasks
without the flag release their references directly, including the last one,
without looking at the FIFO. Submitting a task from another thread does not
set the flag, because its initial reference passes to the owner before the
task can hand out a waker.

Once the flag is set, ordering matters. The owner's final release waits behind
acquisitions that are already queued, and while commands are pending, the
owner's other releases go through the queue too. Otherwise a release could
overtake an earlier queued release and invalidate a later wake or acquisition.
When a remotely cloned waker returns to the owner, the flag store is
synchronized; until then, the waker it was cloned from keeps the allocation
alive. Processing a queued command also synchronizes with the flag store. The
flag is only shared metadata; remote threads never update the reference count.

### Task completion

When a task completes, its future is dropped. Its storage stays allocated while
any native standard waker, local handle, or join handle still points to it,
and a join handle also owns the result until it is taken. This leftover storage
is not a running future. Dropping the join handle frees the result, whether or
not wake handles remain.

The list of unfinished tasks does not own them, so it never keeps an orphaned
future alive.

### Routing remote commands

A global registry maps runtime IDs, which are never reused, to runtimes, so a
standard waker can find where to send its commands. Each remote thread caches
an owned reference to the last runtime endpoint it used. A cache hit skips the
registry lookup and the per-operation reference counting on the queue.

Remote waker commands and unpolled submissions reserve admission before they
publish, so shutdown waits for work that was already accepted, even when the
publisher bypassed the registry. A cached endpoint that has closed rejects new
work. Notifications run only after admission, registry locks, and cache borrows
are released. When the cache evicts an endpoint, it drops the old one outside
any locks or borrows and before admitting the new operation, so driver
destructors can safely re-enter. Wakers used while thread-locals are being
destroyed fall back to the registry.

The owner thread has its own cache: the last runtime ID it checked and whether
that endpoint is open. A hit also proves that the current thread owns the
runtime, which saves a separate thread-ID lookup; a miss checks ownership
before consulting the registry. Shutdown records the closed state in this
cache before running callbacks, and because IDs are never reused, that entry
stays correct. This cache holds no queue reference. Switching between runtimes
on one thread goes back to the registry.

Unpolled `Send` submissions travel through the same FIFO as remote wakes. Once
a task is running, all of its state, references, join results, and awaiters
are owner-local, and local wakes use the intrusive local ready queue. Native
join awaiters hold `LocalWaker`s, which can still be released after they
unregister.

Each task stores a raw pointer to its queue that only the owner thread uses,
rather than a strong or weak reference. Unfinished tasks use it while their
runtime or runner keeps the queue alive; completed tasks need a live endpoint
before they borrow it. Deferred releases on a busy owner publish without taking
a temporary strong queue reference, but releases that may send a notification
or run shutdown cleanup still take one. Cleanup that can run callbacks resolves
the runtime ID to a strong queue reference outside the registry lock.

### Limits and memory layout

Runtime IDs and owner-thread indexes are 16 bits, which keeps the task core at
80 bytes on 64-bit targets. A process can create at most 65,535 native
runtimes, and at most 65,535 threads can own one. Neither is reused, and the
process aborts if either runs out. Threads that only wake tasks or spawn
through a `Handle` do not use an index. The fields used by wakes, runs, and
releases sit in the first 64 bytes of the core, and the task-list links, used
only on spawn, completion, and shutdown, come last. Task allocations are not
cache-line aligned, so this makes single-cache-line access more likely but
does not guarantee it.

After running local tasks, the scheduler checks whether the remote FIFO is
empty before trying to pop from it, avoiding the full memory fence that an
empty pop costs. It still rechecks readiness afterward, so a command published
in the meantime either keeps the runtime polling or wakes the driver. A
non-empty FIFO is drained in order, within the same per-turn budget as before.

### Shutdown

Shutdown closes admission, disables the runtime's wake endpoint, drains
accepted commands, and drops live futures on their owner thread. The closed
registry entry stays available for owner cleanup, including panic cleanup,
until draining finishes. Only then is it removed, so owner releases cannot
overtake pending commands. After that, standard-waker operations are ignored,
and cloning such a waker produces an inert one.

**Standard wakers still alive at shutdown permanently keep their task
allocations**, even after they are dropped. This is retained memory, not
continued execution: task allocations do not keep the driver or the queue's
backing memory alive. Separately, a remote thread's endpoint cache can keep one
closed queue and its driver alive until it caches a different endpoint or the
thread exits. Owner-local handles can still be released after shutdown without
touching the possibly dangling queue pointer. If reclaiming task memory
matters, drop standard wakers and let the owner process their queued releases
before shutdown.

[`ntex_rt::LocalWaker`]: https://docs.ntex.rs/api/ntex_rt/struct.LocalWaker.html
