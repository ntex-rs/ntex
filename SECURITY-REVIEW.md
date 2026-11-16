# Security review: ntex-service and ntex-rt

Scope: use-after-free and undefined behavior in `ntex-service` and `ntex-rt`.

Method: manual review of all `unsafe` code, plus Miri
(`cargo +nightly miri test`) on the `ntex-service` lib tests (170) and the
`ntex-rt` runtime, waker, remote, pool and task tests (164). Findings 1 and 2
were confirmed with dedicated Miri reproductions.

## Findings

| # | Severity | File | Lines | Vulnerability | Confidence |
|---|----------|------|-------|---------------|------------|
| 1 | 🟠 HIGH | ntex-service/src/cfg.rs | 128-193 | `CfgContext` can outlive the thread-local default config | 10/10 |
| 2 | 🟠 HIGH | ntex-service/src/cfg.rs | 264-277 | `Cfg::drop` releases storage through user-provided `ctx()` | 9/10 |
| 3 | 🟡 MEDIUM | ntex-service/src/ctx.rs | 188, 208 | Waker replaced while a slab `&mut` is live | 5/10 |

### 1. `CfgContext` can outlive the thread-local default config

`CfgContext` wraps a raw `*const Storage`, is marked `Send` and `Sync`, and
holds no reference count. `CfgContext::default()` points at the thread-local
`DEFAULT_CFG`. A context, or a configuration built from it (for example
`IoConfig::new()`), can be moved to another thread. Once the original thread
exits, `DEFAULT_CFG` is destroyed and the context dangles.

Miri: "constructing invalid value: encountered a dangling reference
(use-after-free)" when building `&Storage`.

Suggested fix: make `CfgContext` hold a strong `Arc<Storage>`, or keep it
`!Send` and never hand out contexts that point at the thread-local default.

### 2. `Cfg::drop` releases storage through user-provided `ctx()`

`Cfg::drop` calls `Arc::decrement_strong_count(T::ctx().0)`. `ctx()` and
`set_ctx()` come from the safe `Configuration` trait, so a `set_ctx` that
ignores its argument makes `ctx()` return the default context. Every drop then
decrements `DEFAULT_CFG`, which is eventually freed while still in use.

Miri: "dangling reference (use-after-free)" on `ArcInner<Storage>`.

Suggested fix: store the storage pointer that `Cfg<T>` acquired inside `Cfg<T>`
and release that pointer, independent of user code.

### 3. Waker replaced while a slab `&mut` is live (latent)

In `WaitersRef::run_call` and `WaitersRef::run`, `*slot = Some(cx.waker().clone())`
drops the previous waker while `slot` borrows the `UnsafeCell` slab mutably. A
waker whose `Drop` or `Clone` re-enters `bind()` could reallocate the slab under
the live reference.

Not reproduced: the registered `ReadyObserver` holds its own clone of the
waker, and dropping a pending future clears the slot first, so the slot never
holds the last reference in the paths tested.

Suggested fix: `mem::replace` the old waker and drop it after the borrow ends.

## ntex-rt

No use-after-free or undefined behavior was found. Miri reported memory leaks
only, and they are intentional:

- A task dropped on a thread that does not own it, or after its runtime has
  shut down, is leaked instead of freed (`rt/task.rs` `leaked()`, `rt/wake.rs`
  `Owner::Closed`). Its future, output and awaiter may be `!Send`, so freeing it
  on a foreign thread would be unsound.
- Some test runtimes stop with tasks still pending.

Known separately and not re-reviewed here: RT-1 (stall detector unwinds inside
a signal handler).
