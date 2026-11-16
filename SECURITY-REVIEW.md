# Security review: ntex-service, ntex-rt and ntex-bytes

Scope: use-after-free and undefined behavior in `ntex-service`, `ntex-rt` and
`ntex-bytes`. The `ntex-bytes` review is in its own section below.

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

Not reproduced with in-tree wakers: the registered `ReadyObserver` holds its
own clone of the waker, and dropping a pending future clears the slot first, so
the slot never holds the last reference in the paths tested.

Suggested fix: `mem::replace` the old waker and drop it after the borrow ends.

Status: fixed in `057fd32fb`. A waker whose `Clone` re-enters the waiter list
reproduces the use-after-free under Miri. The fix clones and drops wakers
outside the slab borrow.

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

## ntex-bytes

Method: manual review of all `unsafe` code, plus Miri on the full `ntex-bytes`
test suite:

| Configuration | Result |
|---|---|
| Stacked Borrows | 189/189 pass |
| Stacked Borrows, `tracker` feature | 212/212 pass |
| Tree Borrows, `tracker` feature | 212/212 pass |
| `i686-unknown-linux-gnu`, `tracker` feature | 209/209 pass |
| `-Zmiri-many-seeds=0..32` on the 10 multi-threaded tests | No UB or data race |
| `-Zmiri-strict-provenance` | 208/212 pass, see note below |

The finding was confirmed natively and with a Miri reproduction.

| # | Severity | File | Lines | Vulnerability | Confidence |
|---|----------|------|-------|---------------|------------|
| 1 | 🟠 HIGH | ntex-bytes/src/pages.rs | 341-344 | `BytePages::move_to` truncates a current page that is still shared | 10/10 |

### 1. `BytePages::move_to` truncates a current page that is still shared

When the current page is small enough to copy, `move_to` copies it into the
destination and then calls `truncate(0)` on it. `BytePages::clone` and
`copy_to` share the current page through `shallow_freeze`. `StorageVec::truncate`
reclaims the page only when it is unique; otherwise it only sets the length to 0
and leaves the offset unchanged. The next write into `self` then overwrites the
bytes that the cloned `BytePages` or `Bytes` still exposes as immutable data.

```rust
let mut a = BytePages::default();
a.put_slice(&[1; 100]);
let b = a.clone();
let mut c = BytePages::default();
c.put_slice(&[0; 10]);
a.move_to(&mut c);
a.put_slice(&[2; 100]); // `b` now reads [2; 100]
```

Conditions: the shared current page holds more than `INLINE_CAP` bytes (23 on
64-bit targets) and less than 4 KiB, the destination has more spare capacity
than that, and the page is not a `Size4` page.

Impact: safe code mutates memory behind a live `&[u8]`, which is undefined
behavior. If the clone is sent to another thread, it is also a data race.
Natively, a slice borrowed from `b` changes from `1` to `2`. Miri reports a
Stacked Borrows violation on the borrowed slice.

Reach: introduced in `347f9aac6`. No caller in the workspace clones a
`BytePages` before calling `move_to` (ntex-io, ntex-tls and ntex-net use it on
unshared buffers), but `clone`, `copy_to` and `move_to` are public safe APIs.

Suggested fix: copy only when the current page is unique (add
`st.is_unique()` to the `copy` predicate). Alternatively, advance the start
with `set_start(len)` instead of calling `truncate(0)` when the page is shared.

### Note: exposed provenance for external storage

Under `-Zmiri-strict-provenance`, 4 tests fail at `storage.rs:754`:
`st_vtable()` rebuilds the external-storage vtable pointer with
`ptr::with_exposed_provenance`. This is by design, because the pointer is stored
in the `offset` field, and it is sound under the default permissive provenance
model. It is not a finding.
