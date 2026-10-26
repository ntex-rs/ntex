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
with `detach()`, or queried with `is_finished()`. It is **neither `Send` nor
`Sync`**, even when the result is `Send`, and must stay on its owner thread.
A task panic or cancellation is reported as `JoinError`, not as the task's
normal output.

Dropping a local join handle detaches the task; it does not cancel it. Keep and
await the handle when the result matters. A detached task is not guaranteed to
finish before its arbiter or the whole system shuts down.

To submit work to another runtime thread, obtain an arbiter's handle and call
`spawn()` on it. **`Handle::spawn()` returns a separate `RemoteJoinHandle<T>`**,
while local `spawn()` returns `JoinHandle<T>`. Both the submitted future and
output must be `Send + 'static`. Called on the
runtime's own thread, it spawns a local task directly. From other threads the
future, and later its output, share one allocation that is passed to the
runtime thread. The future is polled and dropped there. The handle stays on the
calling thread: it is neither `Send` nor `Sync`. These contracts apply to
native, Tokio, and Compio.

The local handle does not carry the remote-join variant; native local handles
are pointer-sized. `RemoteJoinHandle` retains both paths so same-thread
submissions still avoid a shared remote-job allocation.

```rust
async fn example(handle: ntex_rt::Handle) {
    let task = handle.spawn(async { 42 });
    assert_eq!(task.await.unwrap(), 42);
}
```

Dropping the handle detaches the task, `RemoteJoinHandle::cancel()` cancels it on the
runtime thread. Shutdown or cancellation before producing a result yields
`JoinError`. Rejected submissions are never polled and their futures may be
dropped on the submitting thread.

For fire-and-forget work, use `Handle::spawn_detached()`: it returns `()` and
permits non-`Send` outputs because they are discarded on the runtime thread.

A submitted future can start non-`Send` local work with `ntex::rt::spawn`.
To await its local join, start a local task rather than holding that join across
an `.await` in the submitted `Send` future.

[`JoinHandle`]: https://docs.ntex.rs/api/ntex/rt/struct.JoinHandle.html
[`RemoteJoinHandle`]: https://docs.ntex.rs/api/ntex/rt/struct.RemoteJoinHandle.html
[`ntex::rt::spawn`]: https://docs.ntex.rs/api/ntex/rt/fn.spawn.html

## Owner-thread wake handles

[`ntex_rt::LocalWaker`] is an owned, thread-confined wake handle, not the
notification slot named `ntex::task::LocalWaker`. Native ntex task wakers
acquired on their owner thread use local operations without repeating the
thread-identity check on every clone, wake, or drop. Timers, I/O waiters, and
notification slots retain these handles internally.

Acquisition from a borrowed standard waker checks its representation, owner,
and runtime endpoint, then acquires owner-local wake ownership.
Retaining the handle amortizes that work;
repeatedly acquiring and converting it is not a free optimization.
`into_waker()` consumes the handle to produce a thread-safe standard waker.
Native conversion acquires standard-waker ownership before releasing the local
reference. Local handles cannot themselves cross threads.

On 64-bit targets the handle, its `Option`, and the notification slot each
occupy 16 bytes. Neither native nor fallback handles add a wrapper allocation.
Native tasks coallocate immutable routing metadata, a pinned execution core,
and future/result storage. There is no separate wake-token allocation or
per-task lookup registry. Segments of the remote command queue also consume memory.
Arbitrary waker callbacks may still allocate, as may timer or I/O storage
when it first grows.

This is a hot-path optimization, not a guarantee of faster task scheduling or
request handling. Same-waker registration still has representation-checking
overhead. Tokio, Compio, and wrapped or custom wakers retain their original
callbacks and may incur small additional dispatch costs. Standard native
wakers publish typed reference and wake commands to the owner through a
Crossbeam FIFO. On 64-bit targets each command occupies one pointer-sized word:
three task-pointer alignment bits encode its operation. Encoding preserves pointer
provenance, and decoding removes the tag before any task access. Other targets
retain separate pointer and operation fields without increasing task alignment.
The owner alone updates waker reference counts, including after
completion. Each task has a sticky atomic flag, set before its first remote
waker publication and never cleared. Untouched tasks release directly, including
their final reference, without inspecting or publishing to the FIFO. Remote
submission alone does not set the flag: its initial reference transfers to the
owner before the task can expose a waker.

For touched tasks, a final owner release waits behind already-published
acquisitions. Other owner releases also join the FIFO when commands are pending,
so they cannot invalidate a later wake or acquisition by overtaking earlier
queued releases. Returning a remotely cloned waker to the owner synchronizes
the flag store; until then the publisher's source protects the allocation.
Processing a queued command also acquires the preceding flag store. The flag
is shared metadata, not a remotely updated reference count.

Completion drops the future. Surviving native standard wakers, local handles,
and join handles retain completed task storage; a join handle also
owns its unconsumed result. This retained storage is not a still-running future.
Dropping the join handle releases its result independently of wake handles.

The unfinished-task list is non-owning and does not keep orphaned futures alive.
A global registry routes standard wakers by never-reused runtime IDs. Each remote
thread caches at most one owned endpoint reference; hits avoid registry lookup
and per-operation queue reference counting. Both remote waker commands and unpolled
submissions reserve admission before publication, so shutdown waits for accepted
work even when a publisher bypasses the registry. A cached closed endpoint rejects
new work. Notifications run after releasing admission, registry locks, and cache
borrows. Cache eviction also drops the old endpoint outside locks and borrows,
before admitting the new operation, allowing driver destructors to reenter safely.
Wakers used during thread-local destruction fall back to registry lookup.

Owner-thread operations separately cache the last checked runtime ID and whether its endpoint is open.
A matching cache entry also proves ownership, avoiding a separate thread-ID
lookup; cache misses check ownership before consulting the registry.
Shutdown caches its closed state before running callbacks; never-reused IDs make
that state permanent. The cache holds no queue reference. Switching between
runtimes on one thread checks the registry again.
Unpolled `Send` submissions share the remote wake FIFO; once activated,
all task state, references, join results, and awaiters are owner-local.
Owner-local wakes use the intrusive local ready queue. Native join awaiters
retain `LocalWaker`s and remain releasable after unregistering.
Deferred releases on a busy owner publish without acquiring a temporary strong
queue reference; releases that may notify or run shutdown cleanup still retain it.
Each task stores an owner-only raw queue pointer, not a strong or weak queue
reference. Unfinished tasks access it while their runtime or runner keeps the queue
alive; completed tasks require a live endpoint before borrowing it. Callback-capable
cleanup resolves the runtime ID to a strong queue reference outside the registry lock.

Runtime IDs and owner-thread indexes are 16-bit, keeping the task core at 80
bytes on 64-bit targets. A process can create at most 65,535 native runtimes, and
at most 65,535 threads can own one; neither is reused, and the process aborts when
either runs out. Threads that only wake tasks or spawn through a Handle do not
use an index. Fields used by wakes, runs and releases share the first 64 bytes of
the core; the task list links, used only on spawn, completion and shutdown, come
last. Task allocations are not cache-line aligned, so this reduces rather than
guarantees single-line access.

After running local tasks, the native scheduler checks whether the remote FIFO
is empty before entering its pop loop, avoiding the empty pop's full fence.
It still rechecks readiness afterward; racing publications either keep the
runtime polling or notify the driver. Nonempty drains retain their existing
FIFO ordering and per-turn budget.

Shutdown closes admission and disables the runtime's wake endpoint, drains accepted
commands, and drops live futures on their owner thread. The closed registry entry
remains available to owner cleanup until draining finishes, including panic cleanup.
Only then is the entry removed, so owner releases cannot overtake pending commands.
Later standard-waker operations are
ignored, and cloning such a waker produces an inert waker. **Outstanding standard
wakers at shutdown permanently retain their task allocations**, even after those
wakers are dropped. This is storage retention, not continued execution: task
allocations do not keep the driver or the queue's backing allocation alive.
A remote thread's endpoint cache can independently retain one closed queue and
driver until it caches a different endpoint or the thread exits.
Owner-local handles remain releasable after shutdown without dereferencing the
possibly dangling queue pointer. Release standard wakers and service their queued
releases before shutdown when task-storage reclamation matters.

### Measured costs

The measurements below predate the local/remote join-handle split, the empty-FIFO
fast path, raw task queue pointers, compact task cores, pointer-tagged FIFO
entries, remote endpoint caching, and `LocalWaker` reference-count inlining,
and do not measure their performance.

A release-build comparison against the earlier option-1 LocalWaker prototype,
using matched dependencies and Rust 1.99.1 on x86-64 Linux/WSL2, measured these
allocator-requested sizes:

| Allocation | Earlier prototype | Owner-directed implementation |
| --- | ---: | ---: |
| Empty pending task | 88 bytes | 104 bytes |
| Task with the tested 24-byte future | 104 bytes | 120 bytes |
| Warm runtime, before FIFO segments | 904 bytes | 896 bytes |

These sizes include the merged submission/wake queue and sticky publication
flag. The flag fits existing padding, adding no allocation bytes versus the
same implementation without it. Each tested task still used one allocation.
Local-only final releases needed no FIFO segments; remote FIFO segments required
752 bytes each, with one empty serviced segment retained until runtime
destruction. The shared registry used 80 bytes of static storage and 84 bytes
of retained map capacity with one active runtime. The endpoint cache occupied
16 bytes of thread-local storage per owner thread. The tested `usize` oneshot
channel required a separate 32-byte allocation. These are measurements of
specific types and a specific build, not layout guarantees or RSS estimates.

With pointer-tagged FIFO entries on x86-64, an entry shrinks from 16 to 8 bytes
and a queue segment from 752 to 504 bytes. The 80-byte task core and the queue's
cache-line separation are unchanged.

Owner-only reference counting is not an across-the-board speedup. Across nine
alternating paired runs after local fast-path optimization, active standard-waker
clone/drop increased from 5.12 to 5.58 ns versus the earlier prototype (10%
median paired increase), LocalWaker acquisition/drop from 3.82 to 4.85 ns (25%),
and serviced local spawn/join from 224 to 235 ns (5%). Retained active LocalWaker
clone/drop improved from 4.65 to 2.43 ns (49%). Remote clone/drop with a paused
owner increased from 21 to 123 ns on the producer, plus about 39 ns of deferred
owner work. Backlogged completed standard-waker clone/drop increased from
18 to 57 ns including owner service; the tested 64:1 local/remote mix increased
from 5.4 to 8.4 ns per local operation. These are microbenchmark costs, not
application-throughput predictions.

A separate paired comparison isolating the sticky flag measured serviced local
spawn/join at 325 to 235 ns (27% median paired reduction), detached completion
at 190 to 143 ns (25%), and completed-join drop plus service at 79 to 37 ns
(54%). Remote clone/drop producer cost was essentially unchanged. The tradeoffs
included about 10% more total CPU for backlogged completed standard-waker
clone/drop and 13% more owner CPU in the tested 64:1 local/remote mix.
These are separate paired runs, not an additive breakdown of lifecycle costs.

A subsequent nine-pair comparison isolated the local fast-path refactoring:
untouched releases are inlined, remotely touched releases are outlined, and
endpoint-cache misses are isolated from cache-hit routing. The baseline and
candidate both included the pinned remote-future destruction fix. Active
standard-waker clone/drop improved from 7.05 to 5.62 ns (20% median paired
reduction), retained LocalWaker clone/drop from 4.25 to 2.45 ns (43%), and
I/O-style waker replacement from 11.43 to 9.53 ns (18%). Allocation/retention
probe outputs were identical. These changes preserve all endpoint-liveness,
reference-count and FIFO-ordering checks; the remote-FIFO and backlogged-release
regressions against the earlier prototype remain.

Shutdown retention is cumulative across runtime lifetimes. In the same build,
four runtimes with 32 surviving standard wakers each, targeting futures with
64 KiB inline payloads, permanently retained 8,407,552 requested bytes even
after the wakers were dropped. That older weak-reference implementation included
896 bytes of queue backing per runtime; raw queue pointers no longer retain it.
Future destructors, unconsumed-result destruction on join drop, and
driver destruction still ran; surviving owner-local handles remained reclaimable.

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
