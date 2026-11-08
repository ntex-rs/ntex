# Timers and Time Utilities

A network server is full of deadlines. A connection must finish its TLS
handshake in time, a client that stops sending must eventually be dropped,
an idle keep-alive connection must be closed, and an upstream request must
not wait forever. Almost none of these timers ever fire: the handshake
completes, the next request arrives, the upstream answers. Each one is armed,
then cancelled or pushed further into the future, thousands of times per
second.

`ntex-util` provides the timer functionality for ntex. The `ntex` crate
re-exports it as [`ntex::time`], and it works the same with every runtime
backend. It contains:

- [`Millis`] and [`Seconds`], compact duration types used throughout the ntex
  configuration APIs;
- asynchronous utilities: [`sleep`], [`deadline`], [`timeout`],
  [`timeout_checked`] and [`interval`];
- [`CallbackTimer`], which runs a short closure on expiry without a task;
- [`now`] and [`system_time`], cheap cached clocks.

## How the timer works

The ntex timer implementation is ported from the Linux kernel
[timer wheel](https://github.com/torvalds/linux/blob/master/kernel/time/timer.c).
The kernel faces the same problem as a network server: most of its timers
are timeouts for events that are expected to arrive, such as a network packet
or an I/O completion, and are cancelled long before they expire.
[Reinventing the timer wheel](https://lwn.net/Articles/646950/) on LWN
describes the design and the reasoning behind it.

### A wheel of buckets

Keeping timers in a sorted list or a binary heap gives exact ordering, but
every insert and cancel costs `O(log n)` or more. A **timer wheel** works
more like a clock face. It is an array of buckets, and each bucket collects
all timers that expire during the same slice of time. Adding a timer means
computing a bucket index and pushing an entry; cancelling it means removing
that entry. Both are `O(1)`, however many timers there are.

The wheel doesn't count time in milliseconds but in *units*, and by default
**one unit is 16 milliseconds**. Every delay is converted to units before it
is stored, so one unit is the finest resolution an ntex timer can have. The
unit can be made smaller, see [Choosing the resolution](#choosing-the-resolution).

Each bucket on the first level covers one unit, so 64 buckets reach only
about one second ahead. To cover longer delays, ntex stacks 8 levels of 64
buckets, and each level is 8 times coarser than the one below it. With the
default 16 ms unit:

| Level | Granularity | Range            |
|-------|-------------|------------------|
| 0     | 16 ms       | 0 .. ~1 s        |
| 1     | 128 ms      | ~1 s .. ~8 s     |
| 2     | ~1 s        | ~8 s .. ~64 s    |
| 3     | ~8 s        | ~64 s .. ~8.6 m  |
| 4     | ~65 s       | ~8.6 m .. ~69 m  |
| 5     | ~8.7 m      | ~69 m .. ~9.2 h  |
| 6     | ~70 m       | ~9.2 h .. ~3 d   |
| 7     | ~9.3 h      | ~3 d .. ~24.5 d  |

A timer goes on the first level that can hold its delay: a 100 ms timeout
lands on level 0, a 30-second keep-alive on level 2. Anything longer than
the whole wheel, about 24 days, is clamped to that maximum.

### No cascading

In a classic hierarchical wheel, timers *cascade*: when a coarse bucket
comes due, its timers are moved down into finer levels so that each one
fires at its exact time. That work comes in bursts, and most of it is wasted,
because by then most of those timers would have been cancelled anyway.

The modified wheel, added to Linux in 2016 and used by ntex, skips cascading
entirely. A timer stays in the bucket where it was placed, and its expiry is
**rounded up** to that level's granularity. As a result:

- **A timer never fires early.** The delay counts from the moment the timer
  is armed, and is then rounded up.
- **A timer may fire a little late**, by at most one granularity of its
  level. For delays over one second this is less than about 13%: a 5-second
  timer can be up to 128 ms late, a 30-second timer up to about a second.
- **Every operation is `O(1)`**, including expiry processing.

### Finding the next expiry

Each level keeps a 64-bit bitmap with one bit per non-empty bucket. To find
the next timer that is due, the wheel rotates the bitmap to the current
position and counts the trailing zeros. Empty buckets are never scanned.

### One driver per thread

Every runtime thread, such as each server worker, has its own wheel. Timer
operations therefore need no locks or atomics. This is also why timer
handles are `!Send`: create a timer on the thread that awaits it.

Each thread runs a single driver task. It sleeps until the earliest
non-empty bucket is due, then wakes every timer in that bucket. The driver
moves the wheel clock to the time the bucket was *scheduled* for, not the
time it actually woke up, so one late wakeup doesn't delay the timers that
come after it. The driver starts with the first timer, so a thread that
never uses timers never wakes up for them. When the runtime stops, every
pending timer is marked as elapsed.

### Low precision by design

ntex timers are made for network work: timeouts, keep-alives, retries and
periodic housekeeping. They give up precision in exchange for constant-time
operations. Don't use them to measure time, to rate-limit at millisecond
resolution, or to drive animations. Their resolution is one unit (16 ms by
default), and even a zero-duration [`sleep`] waits for the next timer tick. To measure
how long something took, use [`std::time::Instant`].

The ntex I/O layer goes a step further. Per-connection dispatcher timeouts,
such as keep-alive and frame read rate, are grouped into one-second slots,
and all connections on a thread share a single timer. That's one reason
most I/O configuration methods take [`Seconds`].

### Choosing the resolution

The unit is a per-system setting. It must be a power of two between 1 and
16 milliseconds; other values panic. Set it with
[`Builder::timer_resolution`] when building the system yourself, or with the
`timer_resolution` argument of `#[ntex::main]`:

```rust
#[ntex::main(timer_resolution = 2)]
async fn main() {
    // timers on this system tick every 2 ms
    assert_eq!(ntex::rt::System::current().timer_resolution(), 2);
}
```

```rust
let sys = ntex::rt::System::build()
    .name("server")
    .timer_resolution(2)
    .build(ntex::rt::DefaultRuntime);
```

Every thread of the system, including server workers, reads the setting
when its timer driver starts, and keeps it until the runtime stops and the
wheel is empty. A thread without a running system uses 16 ms.

The level layout stays the same; only the unit changes. All granularities
and ranges in the table above scale with it. With a 2 ms unit, level 0 has
2 ms buckets and covers about 128 ms, and level 1 has 16 ms buckets. The
whole wheel shrinks in the same proportion:

| Unit  | Level 0 range | Whole wheel |
|-------|---------------|-------------|
| 16 ms | ~1 s          | ~24.5 d     |
| 8 ms  | ~512 ms       | ~12 d       |
| 4 ms  | ~256 ms       | ~6 d        |
| 2 ms  | ~128 ms       | ~3 d        |
| 1 ms  | ~64 ms        | ~1.5 d      |

A smaller unit helps services that wait on short delays, for example a
handler that sleeps for 10 ms, which otherwise always waits a full 16 ms
tick. Longer delays gain little, since they land on a higher level with
about the same granularity as before. The cost is more frequent driver
wakeups while short timers are pending, and a shorter maximum delay: longer
timers are clamped to the end of the wheel. Keep the default
unless short sleeps or timeouts show up in your latency.

## `Millis` and `Seconds`

[`std::time::Duration`] is 16 bytes and can represent billions of years.
Network timeouts need neither. ntex uses two small `Copy` types instead:

| Type      | Inner value | Size    | Longest span  |
|-----------|-------------|---------|---------------|
| `Millis`  | `u32`       | 4 bytes | about 49 days |
| `Seconds` | `u16`       | 2 bytes | about 18 hours |

Both are plain tuple structs, so they can be written directly and used in
`const` items. They implement `Eq`, `Ord` and `Hash`, and their default value
is zero:

```rust
use std::time::Duration;
use ntex::time::{Millis, Seconds};

const CONNECT_TIMEOUT: Millis = Millis(5_000);
const KEEP_ALIVE: Seconds = Seconds(30);

fn conversions() {
    assert_eq!(Millis::from_secs(2), Millis(2_000));
    assert_eq!(Millis::from(Seconds(2)), Millis(2_000));
    assert_eq!(Millis::from(Duration::from_millis(1_500)), Millis(1_500));

    let d: Duration = KEEP_ALIVE.into();
    assert_eq!(d, Duration::from_secs(30));
    let d: Duration = CONNECT_TIMEOUT.into();
    assert_eq!(d, Duration::from_secs(5));

    // Mixed arithmetic produces `Millis`.
    assert_eq!(Millis(250) + Seconds(1), Millis(1_250));
    assert_eq!(Duration::from_secs(1) + Millis(250), Duration::from_millis(1_250));
}
```

Conversions and arithmetic **saturate** instead of overflowing or
panicking. A `Duration` or `usize` that does not fit becomes the maximum
value and logs an error:

```rust
use std::time::Duration;
use ntex::time::{Millis, Seconds};

fn saturation() {
    assert_eq!(Millis(u32::MAX) + Millis(1), Millis(u32::MAX));
    assert_eq!(Seconds::checked_new(100_000), Seconds(u16::MAX));
    assert_eq!(Millis::from(Duration::from_secs(u64::MAX)), Millis(u32::MAX));
}
```

`Millis::from(Duration)` rounds any sub-millisecond part **up**, so only
`Duration::ZERO` becomes zero. A short positive timeout stays enabled and
never fires early:

```rust
use std::time::Duration;
use ntex::time::Millis;

fn rounding() {
    assert_eq!(Millis::from(Duration::from_micros(500)), Millis(1));
    assert_eq!(Millis::from(Duration::from_micros(1_500)), Millis(2));
    assert_eq!(Millis::from(Duration::ZERO), Millis::ZERO);
}
```

A negative `i32` is invalid input. It panics in debug builds; release
builds log an error and use zero, which disables the timeout.

[`Millis`] converts from [`Seconds`], `Duration`, `u32`, `i32` and `usize`;
[`Seconds`] converts from `u16`, `i32` and `usize`. Both convert into
`Duration`. There is no conversion from `Duration` to `Seconds`, because it
would silently drop the sub-second part.

### Zero means "disabled"

In ntex configuration, a zero timeout usually turns the feature off: for
example `Seconds::ZERO` as a keep-alive timeout disables keep-alive. The
`is_zero`, `non_zero` and `map` helpers make that convention easy to follow
in your own code. `map` calls the closure only for a non-zero value; on
`Seconds`, the closure receives the value as `Millis`:

```rust
use ntex::time::{Millis, Seconds, Sleep, sleep};

fn idle_timer(timeout: Seconds) -> Option<Sleep> {
    timeout.map(sleep)
}

fn retry_delay(base: Millis, attempt: u32) -> Millis {
    if base.is_zero() {
        Millis::ZERO
    } else {
        Millis(base.0.saturating_mul(attempt))
    }
}
```

### Passing durations to timers

Every timer constructor accepts `impl Into<Millis>`, so the same function
takes any of these types:

```rust
use std::time::Duration;
use ntex::time::{Millis, Seconds, sleep};

async fn waits() {
    sleep(Millis(100)).await;
    sleep(Seconds(1)).await;
    sleep(Duration::from_millis(100)).await;
}
```

## Reading the clock

Reading the system clock is cheap, but not free, and a busy server reads it
for every request: to check deadlines, write a `Date` header or log access
times. `ntex::time` offers two cached clocks:

- [`now`] returns an [`Instant`] for the start of the current runtime turn.
  On the neon and compio backends, all tasks polled in the same turn see the
  same value; on tokio it is [`Instant::now`].
- [`system_time`] returns a cached [`SystemTime`] that is refreshed roughly
  every 150 ms. Use it for wall-clock values that tolerate that lag, such as
  log lines and HTTP `Date` headers.

Both are approximations. When exact elapsed time matters, call
[`Instant::now`] yourself.

## Asynchronous timers

All asynchronous timers are futures that are woken by the timer driver. No
task is spawned per timer, and dropping a timer cancels it.

### `sleep`

[`sleep`] returns a [`Sleep`] future that completes after the given delay:

```rust
use ntex::time::{Millis, sleep};

#[ntex::main]
async fn main() {
    sleep(Millis(100)).await;
    println!("at least 100 ms have elapsed");
}
```

A `Sleep` does not have to be consumed by `.await`. Its methods take
`&self`, so one timer can be waited on, inspected and rearmed repeatedly:

| Method                 | Effect |
|------------------------|--------|
| `wait().await`         | waits without consuming the `Sleep` |
| `reset(dur)`           | rearms the timer to expire `dur` from now, before or after it has elapsed |
| `elapse()`             | completes the timer immediately and wakes the waiting task |
| `is_elapsed()`         | `true` once the timer has fired or was elapsed |
| `poll_elapsed(cx)`     | polls the timer from a hand-written future or service |

Resetting is cheaper than creating a new timer: it moves the existing entry
to another bucket and keeps the registered waker. This makes `Sleep` a good
fit for an idle timeout that restarts on every message:

```rust
use ntex::channel::mpsc;
use ntex::time::{Seconds, sleep};
use ntex::util::{Either, select};

const IDLE: Seconds = Seconds(30);

async fn read_until_idle(rx: mpsc::Receiver<String>) {
    let idle = sleep(IDLE);
    loop {
        match select(rx.recv(), idle.wait()).await {
            Either::Left(Some(msg)) => {
                idle.reset(IDLE);
                println!("received {msg}");
            }
            Either::Left(None) => break, // all senders are gone
            Either::Right(()) => {
                println!("no messages for {} seconds", IDLE.0);
                break;
            }
        }
    }
}
```

`select` polls its first future first, so a message that arrives at the same
moment as the timeout is still processed.

### `deadline`

[`deadline`] works like `sleep`, but treats zero differently: a
zero-duration `sleep` waits for one timer tick, while a zero-duration
[`Deadline`] **never** completes. This fits the "zero means disabled"
convention, because an optional timeout can be awaited without a separate
code path:

```rust
use ntex::channel::mpsc;
use ntex::time::{Seconds, deadline};
use ntex::util::{Either, select};

/// Returns `None` if no message arrives in time. `Seconds::ZERO` waits forever.
async fn recv_within(rx: &mpsc::Receiver<String>, limit: Seconds) -> Option<String> {
    match select(rx.recv(), deadline(limit)).await {
        Either::Left(msg) => msg,
        Either::Right(()) => None,
    }
}
```

`Deadline::reset` takes `&mut self`. Resetting to zero disables the
deadline, and resetting a disabled deadline to a non-zero value arms it
again. `is_elapsed()` reports a disabled deadline as elapsed, meaning that
there is nothing left to wait for.

### `timeout` and `timeout_checked`

[`timeout`] runs a future with a time limit. It returns `Ok` with the
future's output, or `Err(())` if the time ran out first, in which case the
inner future is dropped:

```rust
use ntex::time::{Seconds, sleep, timeout};

#[ntex::main]
async fn main() {
    let fast = timeout(Seconds(1), async { "done" }).await;
    assert_eq!(fast, Ok("done"));

    let slow = timeout(Seconds(1), sleep(Seconds(5))).await;
    assert_eq!(slow, Err(()));
}
```

If the future and the timer become ready during the same poll, the future
wins. Like `sleep`, a zero-duration `timeout` still waits one timer tick, so
it fails almost immediately unless the future is already ready.

[`timeout_checked`] treats zero as "no timeout" and simply runs the future.
Use it when the limit comes from configuration:

```rust
use ntex::time::{Seconds, timeout_checked};

async fn call_upstream(limit: Seconds) -> Result<String, &'static str> {
    timeout_checked(limit, async { String::from("response") })
        .await
        .map_err(|()| "upstream timed out")
}
```

### `interval`

[`interval`] returns an [`Interval`] that ticks repeatedly. The first tick
completes immediately:

```rust
use ntex::time::{Seconds, interval};

async fn report_stats() {
    let ticks = interval(Seconds(10));
    loop {
        ticks.tick().await;
        println!("collecting statistics");
    }
}
```

Each later tick completes one period after the previous tick was
*observed*. If the task is busy and picks up a tick late, the schedule
shifts instead of delivering a burst of missed ticks. That suits periodic
housekeeping, but an `Interval` is not a precise metronome.

`Interval` also implements [`Stream`] with `()` items, and [`poll_tick`] is
available for hand-written futures. Dropping the interval stops it.

### Timers in hand-written futures

`Sleep`, `Deadline` and `Interval` are `Unpin`, and each has a `poll_*`
method that takes `&self`. A service or future can store a timer as a plain
field and poll it from its own `poll` or readiness check:

```rust
use std::{future::Future, pin::Pin, task::Context, task::Poll};
use ntex::time::{Millis, Sleep};

struct Throttle {
    delay: Sleep,
}

impl Future for Throttle {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.delay.poll_elapsed(cx)
    }
}

fn throttle(delay: Millis) -> Throttle {
    Throttle { delay: Sleep::new(delay) }
}
```

## Callback timers

An asynchronous timer only does something when a task polls it. That is
fine for one task waiting for one timer, but it is wasteful when the reaction
to a timeout is tiny, such as setting a flag or waking someone else: you would
need a task, or an extra branch in an existing `select`, just to observe the
timer.

[`CallbackTimer`] runs a closure directly from the timer driver instead.
No task is spawned and nothing is polled. Each timer is a small wheel entry
plus the closure, so it suits per-connection or per-request state.
The ntex [`KeepAlive`] middleware uses one to notice idle services.

### Using a callback timer

`CallbackTimer::new` arms a single invocation of the closure. The timer is
**reusable**: after it fires or is cancelled, `reset` arms another
invocation of the same closure.

```rust
use std::{cell::Cell, rc::Rc};
use ntex::time::{CallbackTimer, Millis, sleep};

#[ntex::main]
async fn main() {
    let fired = Rc::new(Cell::new(0));
    let counter = fired.clone();
    let timer = CallbackTimer::new(Millis(50), move || {
        counter.set(counter.get() + 1);
    });

    sleep(Millis(100)).await;
    assert_eq!(fired.get(), 1);

    // Rearm, then change our mind.
    timer.reset(Millis(50));
    timer.cancel();
    sleep(Millis(100)).await;
    assert_eq!(fired.get(), 1);

    // Rearm again; this time it fires.
    timer.reset(Millis(50));
    sleep(Millis(100)).await;
    assert_eq!(fired.get(), 2);
}
```

| Method         | Effect |
|----------------|--------|
| `new(dur, f)`  | arms one invocation of `f` after `dur` |
| `reset(dur)`   | arms a new invocation, replacing any pending one |
| `cancel()`     | disarms the timer and keeps the closure for later |
| `is_elapsed()` | `true` when disarmed: fired, cancelled or stopped by shutdown |
| drop           | cancels any pending invocation and releases the closure |

The handle must be kept alive for as long as the callback should run.
Dropping it is the way to stop the timer for good.

### Lazy rearming

The real strength of callback timers is that a frequently extended deadline
does not need a timer operation on every extension. Record the activity time
in a `Cell`, and let the callback check whether the deadline really passed.
If not, it rearms itself for the remaining time. This is how [`KeepAlive`]
works:

```rust
use std::{cell::Cell, rc::Rc, rc::Weak, time::Duration, time::Instant};
use ntex::time::{CallbackTimer, Millis, now};

struct IdleTracker {
    timeout: Millis,
    last_seen: Cell<Instant>,
    expired: Cell<bool>,
    timer: CallbackTimer,
}

impl IdleTracker {
    fn new(timeout: Millis) -> Rc<Self> {
        Rc::new_cyclic(|this: &Weak<IdleTracker>| {
            let this = this.clone();
            IdleTracker {
                timeout,
                last_seen: Cell::new(now()),
                expired: Cell::new(false),
                timer: CallbackTimer::new(timeout, move || {
                    if let Some(tracker) = this.upgrade() {
                        tracker.check();
                    }
                }),
            }
        })
    }

    /// Called on every request. It only stores the time; the timer is untouched.
    fn touch(&self) {
        self.last_seen.set(now());
    }

    fn check(&self) {
        let deadline = self.last_seen.get() + Duration::from(self.timeout);
        let current = now();
        if deadline <= current {
            self.expired.set(true);
        } else {
            self.timer.reset(Millis::from(deadline - current));
        }
    }
}
```

A connection that receives a thousand requests within one timeout period
costs a thousand `Cell` writes and at most one or two timer resets.

The callback captures a `Weak` reference. The tracker owns the timer and the
timer owns the closure, so a strong `Rc` in the closure would form a cycle
and the tracker would never be dropped.

### Rules for callbacks

- **Keep callbacks short, synchronous and non-blocking.** They run inside the
  thread's timer driver; a slow callback delays every other timer on that
  thread. To do asynchronous work, wake a task, notify a waiter or send to
  a channel from the callback.
- **The closure is `Fn`, not `FnMut`,** because it can run many times. Keep
  changing state in `Cell`, `RefCell` or similar.
- **Callbacks may use timers freely.** They run after the driver has released
  the wheel, so a callback can create, reset, cancel or drop timers,
  including its own. A callback that resets its own timer arms a new
  invocation.
- **Order is not guaranteed** between callbacks that fire on the same tick.
- **A zero duration never runs the callback inline;** it waits for the next
  timer tick, like `sleep`.
- **Panics propagate** through the runtime and may stop the timer driver of
  that thread. Do not use panics for control flow in callbacks.
- **Callback timers are local.** The handle is neither `Send` nor `Sync`
  and cannot be cloned, so the closure may capture `Rc` state.
- **Shutdown cancels pending callbacks.** A surviving handle can be reset in
  a new runtime on the same thread, but it is not rearmed automatically.
- `is_elapsed()` tells you that the timer is disarmed, not that the callback
  has already run. Track that in your own state if it matters.

## Summary

- Use [`Millis`] and [`Seconds`] for timeouts. They are small, `Copy`,
  saturate instead of overflowing, and convert to and from `Duration`.
- Use [`sleep`] for a delay, and `reset` a `Sleep` for a restartable idle
  timer.
- Use [`deadline`] or [`timeout_checked`] when zero should mean "no limit",
  and [`timeout`] when zero should fail fast.
- Use [`interval`] for periodic housekeeping, not for precise scheduling.
- Use [`CallbackTimer`] when the reaction to a timeout is a short,
  synchronous state change, especially for per-connection timers that are
  extended often.
- Expect timers to fire on time or a little late, never early. Measure
  elapsed time with `Instant`.
- Keep the default 16 ms timer resolution unless short delays dominate your
  latency; then lower it with `timer_resolution`.

[`ntex::time`]: https://docs.ntex.rs/api/ntex/time/index.html
[`Builder::timer_resolution`]: https://docs.ntex.rs/api/ntex/rt/struct.Builder.html#method.timer_resolution
[`CallbackTimer`]: https://docs.ntex.rs/api/ntex/time/struct.CallbackTimer.html
[`Deadline`]: https://docs.ntex.rs/api/ntex/time/struct.Deadline.html
[`deadline`]: https://docs.ntex.rs/api/ntex/time/fn.deadline.html
[`Interval`]: https://docs.ntex.rs/api/ntex/time/struct.Interval.html
[`interval`]: https://docs.ntex.rs/api/ntex/time/fn.interval.html
[`KeepAlive`]: https://docs.ntex.rs/api/ntex/util/keepalive/struct.KeepAlive.html
[`Millis`]: https://docs.ntex.rs/api/ntex/time/struct.Millis.html
[`now`]: https://docs.ntex.rs/api/ntex/time/fn.now.html
[`poll_tick`]: https://docs.ntex.rs/api/ntex/time/struct.Interval.html#method.poll_tick
[`Seconds`]: https://docs.ntex.rs/api/ntex/time/struct.Seconds.html
[`Sleep`]: https://docs.ntex.rs/api/ntex/time/struct.Sleep.html
[`sleep`]: https://docs.ntex.rs/api/ntex/time/fn.sleep.html
[`Stream`]: https://docs.ntex.rs/api/ntex/util/trait.Stream.html
[`system_time`]: https://docs.ntex.rs/api/ntex/time/fn.system_time.html
[`timeout`]: https://docs.ntex.rs/api/ntex/time/fn.timeout.html
[`timeout_checked`]: https://docs.ntex.rs/api/ntex/time/fn.timeout_checked.html
[`Instant`]: https://doc.rust-lang.org/std/time/struct.Instant.html
[`Instant::now`]: https://doc.rust-lang.org/std/time/struct.Instant.html#method.now
[`std::time::Instant`]: https://doc.rust-lang.org/std/time/struct.Instant.html
[`std::time::Duration`]: https://doc.rust-lang.org/std/time/struct.Duration.html
[`SystemTime`]: https://doc.rust-lang.org/std/time/struct.SystemTime.html
