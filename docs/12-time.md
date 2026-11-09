# Timers and Time Utilities

A network server needs deadlines for TLS handshakes and clients that stop
sending. It must also close idle keep-alive connections and limit waits for
upstream requests.

Almost none of these timers fire. The handshake completes, the next request
arrives, or the upstream answers. Timers are armed, then cancelled or pushed
further into the future, thousands of times per second.

`ntex-util` provides ntex's timer utilities. The `ntex` crate re-exports them as
[`ntex::time`], with the same behavior on every runtime backend. Use [`sleep`]
to delay a task, [`timeout`] to put a limit on another future, or [`interval`]
for periodic work. Their shared timer design explains the trade-off between
precision and cost.

## How the timer works

ntex's timer design makes it cheap to cancel timers before they expire. It is
ported from the Linux kernel
[timer wheel](https://github.com/torvalds/linux/blob/master/kernel/time/timer.c).

The kernel faces the same problem as a network server. Most timers wait for an
expected event, such as a network packet or an I/O completion. They are
cancelled long before they expire.
[Reinventing the timer wheel](https://lwn.net/Articles/646950/) on LWN explains
the design and its reasoning.

### A wheel of buckets

A **timer wheel** groups timers by expiry time, so adding and cancelling timers
costs `O(1)` regardless of their number. A sorted list or binary heap gives
exact ordering, but every insert and cancel costs `O(log n)` or more.

The wheel is an array of buckets, like a clock face. Each bucket holds timers
that expire during the same slice of time. To add a timer, the wheel computes
a bucket index and pushes an entry. To cancel it, the wheel removes that entry.

The wheel counts time in *units*. By default, **one unit is 16 milliseconds**.
Every delay is converted to units before storage. One unit is therefore the
finest resolution an ntex timer can have. You can make the unit smaller; see
[Choosing the resolution](#choosing-the-resolution).

Each bucket on the first level covers one unit. Its 64 buckets reach only
about one second ahead. To cover longer delays, ntex uses 8 levels of 64
buckets. Each level is 8 times coarser than the one below it.

With the default 16 ms unit, the levels cover these ranges:

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

A timer goes on the first level that can hold its delay. A 100 ms timeout
lands on level 0; a 30-second keep-alive lands on level 2. Delays longer than
the whole wheel, about 24 days, are clamped to that maximum.

### No cascading

ntex leaves each timer in its original bucket, avoiding work to move it
between levels. Its expiry is **rounded up** to that level's granularity.
This modified wheel was added to Linux in 2016.

In a classic hierarchical wheel, timers *cascade*. When a coarse bucket comes
due, its timers move into finer levels so that each fires at its exact time.
That work comes in bursts. Most of it is wasted because most of those timers
would have been cancelled before expiry anyway.

For delays within the wheel's range, this rounding means a timer never fires
early: its delay counts from when it is armed and is rounded up. The rounding
adds at most one granularity of its level, less than about 13% for delays over
one second. It can add up to 128 ms to a 5-second timer, or about a second to
a 30-second timer. A busy runtime can delay execution further.

Insertion, cancellation, and per-timer expiry bookkeeping stay `O(1)`.
Expiring a bucket still requires visiting every timer in it, but no timers
need to move between levels first.

### Finding the next expiry

A mostly idle server should not wake up to inspect empty time slots. The
wheel finds the next expiry without scanning empty buckets: each level keeps
a 64-bit bitmap with one bit per non-empty bucket. It rotates that bitmap to
the current position, then counts trailing zeros to find the next timer due.

### One driver per thread

Create a timer on the thread that awaits it. Every runtime thread, including
each server worker, has its own wheel. Timer operations need no locks or
atomics, and timer handles are `!Send`.

One driver task per thread serves all timers on its wheel. It sleeps until
the earliest non-empty bucket is due, then wakes every timer in that bucket.
The driver advances the wheel clock to the bucket's *scheduled* time, even
if it woke up late, so one late wakeup does not shift later timers' deadlines.

The driver starts with the first timer. A thread that never uses timers never
wakes up for them. When the runtime stops, every pending timer is marked as
elapsed.

### Low precision by design

Use ntex timers for network work such as timeouts, keep-alives, retries, and
periodic housekeeping. They trade precision for constant-time operations.
Use [`std::time::Instant`] to measure elapsed time.

Timer resolution is one unit, or 16 ms by default. Even a zero-duration
[`sleep`] waits for the next timer tick. Do not use these timers for
millisecond rate limits or animations.

The ntex I/O layer groups per-connection dispatcher timeouts into one-second
slots. These include keep-alive and frame read rate timeouts. All connections
on a thread share a single timer. This is one reason most I/O configuration
methods take [`Seconds`].

### Choosing the resolution

Keep the default resolution unless short sleeps or timeouts affect your
latency. A smaller unit helps with short delays, at the cost of more frequent
driver wakeups while short timers are pending. It also shortens the maximum
delay; longer timers are clamped to the end of the wheel.

The unit is a per-system setting. It must be a power of two between 1 and
16 milliseconds; other values panic. Use the `timer_resolution` argument of
`#[ntex::main]` to set it:

```rust
#[ntex::main(timer_resolution = 2)]
async fn main() {
    // timers on this system tick every 2 ms
    assert_eq!(ntex::rt::System::current().timer_resolution(), 2);
}
```

When you build the system yourself, use [`Builder::timer_resolution`]:

```rust
let sys = ntex::rt::System::build()
    .name("server")
    .timer_resolution(2)
    .build(ntex::rt::DefaultRuntime);
```

Every thread in the system, including server workers, reads the setting when
its timer driver starts. It keeps that setting until the runtime stops and
the wheel is empty. A thread without a running system uses 16 ms.

The level layout stays the same. All granularities and ranges in the table
above scale with the unit. With a 2 ms unit, level 0 has 2 ms buckets and
covers about 128 ms. Level 1 has 16 ms buckets, and the whole wheel shrinks
in the same proportion:

| Unit  | Level 0 range | Whole wheel |
|-------|---------------|-------------|
| 16 ms | ~1 s          | ~24.5 d     |
| 8 ms  | ~512 ms       | ~12 d       |
| 4 ms  | ~256 ms       | ~6 d        |
| 2 ms  | ~128 ms       | ~3 d        |
| 1 ms  | ~64 ms        | ~1.5 d      |

For example, a 10 ms sleep is scheduled on 16 ms ticks at the default
resolution. A smaller unit reduces the rounding added to that short delay.
Longer delays gain little because they land on a higher level with about the
same granularity as before.

## `Millis` and `Seconds`

[`Millis`] and [`Seconds`] are compact `Copy` duration types used throughout
ntex's configuration APIs. Network timeouts do not need the 16 bytes or the
billions-of-years range of [`std::time::Duration`]:

| Type      | Inner value | Size    | Longest span  |
|-----------|-------------|---------|---------------|
| `Millis`  | `u32`       | 4 bytes | about 49 days |
| `Seconds` | `u16`       | 2 bytes | about 18 hours |

Both types are plain tuple structs. You can write them directly and use them
in `const` items. They implement `Eq`, `Ord`, and `Hash`. Their default value
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

Conversions and arithmetic **saturate** rather than overflowing or panicking.
Converting a `Duration` or `usize` that does not fit gives the maximum value
and logs an error:

```rust
use std::time::Duration;
use ntex::time::{Millis, Seconds};

fn saturation() {
    assert_eq!(Millis(u32::MAX) + Millis(1), Millis(u32::MAX));
    assert_eq!(Seconds::checked_new(100_000), Seconds(u16::MAX));
    assert_eq!(Millis::from(Duration::from_secs(u64::MAX)), Millis(u32::MAX));
}
```

`Millis::from(Duration)` rounds any sub-millisecond part **up**. Only
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

A negative `i32` is invalid input and panics in debug builds. Release builds
log an error and use zero, which disables the timeout.

The supported conversions are:

- `Millis` converts from `Seconds`, `Duration`, `u32`, `i32`, and `usize`.
- `Seconds` converts from `u16`, `i32`, and `usize`.
- Both types convert into `Duration`.

There is no conversion from `Duration` to `Seconds`. It would silently drop
the sub-second part.

### Zero means "disabled"

A zero timeout usually turns off a feature or deadline in ntex configuration,
but the meaning depends on the setting. For example, zero disables the idle
timer in `IoConfig`, while `HttpServiceConfig::set_keepalive_timeout` with
`Seconds::ZERO` disables HTTP persistent connections.

Use the `is_zero` and `non_zero` helpers to follow this convention in your own
code. The `map` helper calls a closure only for a non-zero value. When you call
it on `Seconds`, the closure receives the value as `Millis`:

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

The convenience functions `sleep`, `deadline`, `interval`, `timeout`, and
`timeout_checked` accept `impl Into<Millis>`, so you can pass any supported
duration type directly. The corresponding `Sleep`, `Deadline`, and `Interval`
constructors take `Millis` instead. For example:

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

Use the cached clocks in `ntex::time` when you can tolerate approximate time.
Reading the system clock is cheap, but a busy server pays that cost on every
request. It checks deadlines, writes a `Date` header, or logs access times.

For deadline checks, [`now`] returns an [`Instant`] for the start of the
current runtime turn. On the neon and compio backends, all tasks polled in
the same turn see the same value. On tokio, it is [`Instant::now`].

For wall-clock values, [`system_time`] returns a cached [`SystemTime`],
refreshed roughly every 150 ms. Use it where that lag is acceptable, such as
log lines and HTTP `Date` headers. These clocks favor inexpensive reads over
precision; call [`Instant::now`] yourself when exact elapsed time matters.

## Asynchronous timers

Use asynchronous timers when a task needs to wait for time to pass. Each timer
is a future woken by the shared timer driver, not a separate task. Dropping
a timer cancels it.

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

You can reuse a `Sleep` without consuming it through `.await`. Its methods
take `&self`, so you can wait on it, inspect it, and rearm it repeatedly:

| Method                 | Effect |
|------------------------|--------|
| `wait().await`         | waits without consuming the `Sleep` |
| `reset(dur)`           | rearms the timer to expire `dur` from now, before or after it has elapsed |
| `elapse()`             | completes the timer immediately and wakes the waiting task |
| `is_elapsed()`         | `true` once the timer has fired or was elapsed |
| `poll_elapsed(cx)`     | polls the timer from a hand-written future or service |

Reset a `Sleep` for an idle timeout that restarts on every message. Resetting
moves the existing entry to another bucket and keeps the registered waker.
It is cheaper than creating a new timer:

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

Use [`deadline`] when a zero duration should disable a timeout. It works like
`sleep` for non-zero durations. A zero-duration [`Deadline`] **never**
completes; a zero-duration `sleep` waits for one timer tick.

You can await an optional timeout without a separate code path:

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

`Deadline::reset` takes `&mut self`. Resetting to zero disables the deadline.
Resetting a disabled deadline to a non-zero value arms it again.

`is_elapsed()` agrees with awaiting the deadline. It is `true` only once an
armed deadline has fired, and always `false` for a disabled one.

### `timeout` and `timeout_checked`

Use [`timeout`] to run a future with a time limit. It returns `Ok` with the
future's output if the future finishes in time. If time runs out first, it
returns `Err(())` and drops the inner future:

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

If the future and timer become ready during the same poll, the future wins.
Like `sleep`, a zero-duration `timeout` waits one timer tick. It fails almost
immediately unless the future is already ready.

Use [`timeout_checked`] when the limit comes from configuration. It treats
zero as "no timeout" and runs the future without a time limit:

```rust
use ntex::time::{Seconds, timeout_checked};

async fn call_upstream(limit: Seconds) -> Result<String, &'static str> {
    timeout_checked(limit, async { String::from("response") })
        .await
        .map_err(|()| "upstream timed out")
}
```

### `interval`

For recurring work such as collecting statistics, use [`interval`]. It returns
an [`Interval`] that ticks repeatedly, with the first tick completing
immediately:

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

Each later tick completes one period after the previous tick was *observed*.
If a busy task picks up a tick late, the schedule shifts. It does not deliver
a burst of missed ticks. Use this for periodic housekeeping rather than
precise timing.

`Interval` also implements [`Stream`] with `()` items. Use [`poll_tick`] in
hand-written futures. Dropping the interval stops it.

### Timers in hand-written futures

You can store a timer as a plain field in a service or future. Poll it from
your own `poll` method or readiness check. `Sleep`, `Deadline`, and `Interval`
are `Unpin`; each has a `poll_*` method that takes `&self`:

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

Use [`CallbackTimer`] when expiry needs only a short action, such as setting a
flag or waking another task. It runs a closure directly from the timer driver.
No task is spawned and nothing is polled.

This avoids a task or an extra branch in an existing `select` just to observe
expiry. A callback timer needs only a small wheel entry and the closure, so
it suits per-connection or per-request state.

The ntex [`KeepAlive`] middleware uses a callback timer to notice idle services.

### Using a callback timer

Call `CallbackTimer::new` to arm a single invocation of a closure. Keep the
handle alive for as long as you want the callback to run. The timer is
**reusable**: after it fires or is cancelled, `reset` arms another invocation
of the same closure.

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

### Lazy rearming

On a busy connection, most activity only pushes the idle deadline further
away. You can avoid changing the timer on every request by recording the
activity time in a `Cell`. When the callback runs, it checks whether the
deadline has passed and rearms itself for the remaining time if needed.
This is how [`KeepAlive`] works:

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

For a thousand requests within one timeout period, a connection needs a
thousand `Cell` writes and at most one or two timer resets.

The callback captures a `Weak` reference to avoid a cycle. The tracker owns
the timer, and the timer owns the closure. A strong `Rc` in the closure would
keep the tracker from ever being dropped.

### Rules for callbacks

Callbacks run inside the thread's timer driver, so a slow callback delays
every other timer on that thread. Keep callbacks short, synchronous, and
non-blocking. To do asynchronous work, wake a task, notify a waiter, or send
to a channel from the callback.

The closure implements `Fn`, not `FnMut`, because it can run many times.
Store changing state in `Cell`, `RefCell`, or a similar type. Capturing `Rc`
state is fine: the timer handle is local, neither `Send` nor `Sync`, and
cannot be cloned.

The driver releases the wheel before invoking a callback, so the callback
can create, reset, cancel, or drop timers, including its own. Resetting its
own timer arms a new invocation.

A zero duration still waits for the next timer tick, like `sleep`; it never
runs the callback inline. Do not depend on an order between callbacks due on
the same tick.

Panics propagate through the runtime and may stop that thread's timer driver.
Do not use them for control flow in callbacks.

Shutdown cancels pending callbacks. A surviving handle can be reset in a new
runtime on the same thread, but is not rearmed automatically. As the method
table above shows, `is_elapsed()` reports that the timer is disarmed, not
whether the callback ran. Track execution in your own state if it matters.

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
