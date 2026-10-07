# Byte Buffers

When a socket read contains two messages, a decoder needs to hand the first
message to the application while keeping the second for later. Copying each
message into a new `Vec<u8>` works, but adds allocations and memory copies to
every request. `ntex-bytes` lets the decoder hand out an owned view instead.
The view keeps the data alive without borrowing the decoder's read buffer.

The same idea helps on the write side: a small header and a large body can
be queued separately, rather than copied into one growing buffer. For tiny
values, sharing would cost more than copying, so the crate stores short byte
strings directly in their handles. For frequently reused I/O buffers,
per-thread page caches reduce trips to the allocator.

The `ntex` crate re-exports the main types in [`ntex::util`]: [`Bytes`],
[`BytesMut`], [`ByteString`], [`BytePages`], [`BytePage`], [`BytePageSize`],
[`Buf`] and [`BufMut`]. Page cache tuning, [`set_page_cache_size`], is
available from `ntex_bytes` directly.

## The mental model

Start with three types. Use `BytesMut` while filling or changing a contiguous
buffer, `Bytes` once data is ready to share, and `BytePages` when output can
remain in separate chunks. `ByteString` is the text version of `Bytes`: it
adds the guarantee that the bytes are valid UTF-8.

The important distinction is between a **handle** and its **allocation**.
Several handles can keep one allocation alive, but they do not all have
permission to change it. A `BytesMut` owns the writable tail; `Bytes` views
can own earlier, read-only portions. Splitting off a message gives it an
independent lifetime, not necessarily independent storage.

Here, "zero-copy" means avoiding a copy between application buffers. It
does not promise that the operating system or a TLS layer will avoid copying.
It is also an optimization, not a rule: copying a handful of bytes is often
cheaper than maintaining another shared reference.

If you remember only one rule from this chapter, make it this:

> Share data while it is moving through the pipeline; copy or trim the small
> part that needs to live much longer than the buffer it came from.

## Types at a glance

| Type             | Mutability | Clone cost             | Typical use                                   |
|------------------|------------|------------------------|-----------------------------------------------|
| [`Bytes`]        | Immutable  | Usually shares storage or copies inline data; external storage decides | Decoded frames, payloads, header values |
| [`BytesMut`]     | Unique     | Copies the data        | Read buffers, building output                 |
| [`ByteString`]   | Immutable  | Same as `Bytes`        | UTF-8 text: header names, paths, topics       |
| [`BytePages`]    | Unique     | Shares or copies according to page storage and append rules | Write queues |
| [`BytePage`]     | Immutable  | Shares data, `Vec` pages copy | One chunk of a `BytePages` queue       |
| [`BytePageSize`] | -          | -                      | Allocation size classes                       |

## `Bytes`

[`Bytes`] is an immutable view into contiguous memory. Heap-backed values
normally share a reference-counted allocation when cloned, sliced or split.
Small values and static data use simpler representations, described below.
`Bytes` is `Send + Sync`, so it can be passed between threads.

```rust
use ntex_bytes::Bytes;

let mut msg = Bytes::copy_from_slice(&[1u8; 1024]);

// These two views share the original allocation.
let first = msg.split_to(256);
assert_eq!(first.len(), 256);
assert_eq!(msg.len(), 768);

let part = msg.slice(100..200); // shares the allocation too
let cloned = msg.clone();      // another handle, not another 768-byte buffer
drop(msg);
assert_eq!(part.len(), 100);   // the other handles still own their data
assert_eq!(cloned.len(), 768);
```

A `Bytes` value uses one of four storage kinds:

| Kind     | Created by                                         | Clone                         |
|----------|----------------------------------------------------|-------------------------------|
| Inline   | Data of at most 23 bytes (11 on 32-bit targets)    | Copies the bytes, no allocation |
| Static   | [`Bytes::from_static`], `From<&'static [u8]>`, `From<&'static str>` | Copies the pointer |
| Shared   | A heap buffer, usually from [`BytesMut`]            | Increments the reference count |
| External | [`Bytes::from_ext`], `Arc<str>` via [`ByteString`]  | Delegated to the owner's vtable |

Inline storage keeps small values inside the `Bytes` struct itself.
Operations such as [`Bytes::slice`], [`Bytes::split_to`],
[`BytesMut::split_to`] and [`BytesMut::freeze`] copy small results inline
instead of taking another reference to the heap buffer. A short header name
can therefore outlive a request without keeping its read buffer alive.
[`Bytes::is_inline`] reports the storage kind. Static constructors keep their
static representation even for short strings.

The examples below assume a 64-bit target, where the inline limit is 23 bytes.
On 32-bit targets it is 11 bytes. These limits and the layouts later in the
chapter describe the current implementation, not a stable memory-layout API.

### When sharing retains too much memory

A view that is too large to inline keeps the whole allocation alive, even
if it uses only a small part. Keeping a 100-byte token from a 16 KiB read
buffer in a long-lived cache can therefore retain far more than 100 bytes.
That is not a leak: the allocation is still owned. It is a lifetime tradeoff.

[`Bytes::trimdown`] helps at this boundary. It moves data inline when it
fits, or copies it into an exactly sized buffer when at least 64 bytes of
the backing capacity are unused. Inline and static values are left alone.

```rust
use ntex_bytes::Bytes;

let packet = Bytes::copy_from_slice(&[b'x'; 16 * 1024]);
let mut token = packet.slice(100..200);
token.trimdown(); // copy 100 bytes so `token` no longer retains the packet
drop(packet);
assert_eq!(token.len(), 100);
```

Trimming one view does not release the original allocation if other handles
still own it. Use this deliberately for long-lived values, rather than
trimming every frame and losing the benefit of sharing.

### Externally owned data

External storage lets `Bytes` share memory owned by something else. A type
implements the unsafe [`StorageExt`] trait and provides a static vtable with
`as_ptr`, `len`, `clone` and `drop` functions. A `clone` that returns `None`
makes clones copy the data into native storage instead. `Arc<str>`
implements it, so `ByteString::from(Arc<str>)` shares the string without
copying it. This is an integration API, not something a normal decoder needs
to implement. The unsafe contract requires the exposed bytes to remain valid
and immutable for every live handle, including across threads; the vtable
must clone and release ownership correctly.

## `BytesMut`

[`BytesMut`] is a unique, growable view into a heap buffer, similar to
`Vec<u8>`. Only the `BytesMut` handle can write into its part of the buffer,
but the same allocation can be shared with `Bytes` values split off its
front. Think of the allocation as a line moving through a decoder:

```text
| completed frame | completed frame | unread data | spare capacity |
|<------ immutable Bytes views ---->|<--------- BytesMut -------->|
```

Only one `BytesMut` can own the writable part of a given allocation. Once a
prefix is handed out as `Bytes`, the mutable view advances past it and cannot
overwrite it. The decoder can continue filling the tail while the application
reads earlier frames, without locks around the data.

```rust
use ntex_bytes::{BufMut, BytesMut};

let mut buf = BytesMut::with_capacity(1024);
buf.put_slice(b"hello ");
buf.put_u16(0x1234);
buf.extend_from_slice(b"world");

// 6 bytes fit inline, so `head` is a copy and `buf` stays unique
let head = buf.split_to(6);
assert!(head.is_inline());
assert!(buf.is_unique());

buf.extend_from_slice(&[0u8; 100]);
let body = buf.take(); // shares the allocation with `buf`
assert!(!buf.is_unique());

drop(body);
assert!(buf.is_unique());

// a unique buffer gets its whole capacity back
buf.clear();
assert_eq!(buf.capacity(), 1024);
```

The main operations:

| Method                         | Effect                                                     |
|--------------------------------|------------------------------------------------------------|
| [`BytesMut::split_to`]         | Returns the first `at` bytes as `Bytes`; shares large results and copies small ones inline |
| [`BytesMut::take`]             | Returns all data as `Bytes`, keeping the spare capacity for `self` |
| [`BytesMut::freeze`]           | Consumes the mutable handle; reuses heap storage or copies a small result inline |
| [`BytesMut::advance_to`]       | Drops the first `cnt` bytes, `O(1)`                         |
| [`BytesMut::clear`]            | Empties the buffer, reclaims the full capacity if unique   |
| [`BytesMut::reserve`]          | Ensures spare capacity, see [Growth](#growth)               |
| [`BytesMut::is_unique`]        | `true` if no `Bytes` shares the allocation                  |
| `BytesMut::from(Bytes)`        | Reuses a unique shared heap allocation; copies inline, static, external or still-shared data |

[`BytesMut::new`] currently starts with 112 bytes of data capacity (a
128-byte allocation including its header),
[`BytesMut::with_capacity`] allocates exactly the requested capacity, and
[`BytesMut::with_page_size`] takes a pooled page, see
[Pooled buffers](#pooled-buffers). Cloning a `BytesMut` always copies the
data.

### Length, capacity and spare space

`len()` counts initialized bytes. `capacity()` counts what the current
mutable view can hold, including those bytes, and the difference is spare
space at the end. Capacity is not necessarily the size of the whole
allocation: splitting or advancing the front shrinks the mutable view.

Dropping a frame makes its space eligible for reuse, but does not immediately
move the read cursor backward. A later `clear()` reclaims the whole allocation
if it is unique; `reserve()` can move unread data to the front when more space
is needed. If another handle is still alive, that memory cannot be overwritten.

`clear()` means "discard my data", not "free my allocation". This is useful
for a buffer that will be filled again. Drop the buffer when it is no longer
needed; whether its memory is freed or cached depends on how it was allocated.

### A buffer's typical journey

In a protocol decoder, one allocation often goes through the same cycle many
times:

1. The transport writes socket data into the spare tail of a `BytesMut`.
2. The decoder examines the initialized prefix.
3. A complete frame is removed with `split_to`.
4. The returned `Bytes` travels through services and application code.
5. The `BytesMut` stays with the connection and receives more input.
6. When all shared frames are dropped, `reserve` or `clear` can reclaim the
   space at the front.

This is why the mutable buffer can remain useful even while immutable frames
are alive: they own disjoint views. It is also why holding one frame for a
long time can change allocation behavior for that connection. The decoder
remains correct, but it may need another allocation when the tail fills.

`BytesMut` implements both the ntex [`BufMut`] trait and the `BufMut` trait
of the `bytes` crate. They differ in one detail: ntex's
`BufMut::remaining_mut()` reports the current spare capacity, while the
`bytes` version reports `usize::MAX - len` because the buffer grows on demand.
All `put_*` methods reserve capacity as needed in both cases.

### Zero-copy decoding

Splitting frames off the read buffer is the core pattern of ntex codecs. The
decoder below parses a two-byte, big-endian length followed by a payload.
Large frames share the read buffer; tiny ones are copied inline:

```rust
use ntex_bytes::{Bytes, BytesMut};

fn decode(src: &mut BytesMut) -> Option<Bytes> {
    if src.len() < 2 {
        return None;
    }
    let len = u16::from_be_bytes([src[0], src[1]]) as usize;
    if src.len() < 2 + len {
        // make room for the rest of the frame
        src.reserve(2 + len - src.len());
        return None;
    }
    src.advance_to(2);
    Some(src.split_to(len))
}

let mut src = BytesMut::copy_from_slice(b"\0\x03one");
assert_eq!(decode(&mut src).unwrap(), b"one"[..]);
assert!(src.is_empty());
```

The length field is treated as untrusted input. The decoder checks that the
complete frame is present before calling `advance_to` and `split_to`, because
those methods panic when asked to move past the end. If an index is not already
validated, use [`BytesMut::split_to_checked`] instead:

```rust
use ntex_bytes::{Bytes, BytesMut};

fn take_prefix(src: &mut BytesMut, len: usize) -> Option<Bytes> {
    src.split_to_checked(len)
}

let mut src = BytesMut::copy_from_slice(b"hello");
assert_eq!(take_prefix(&mut src, 2).unwrap(), b"he"[..]);
assert_eq!(src, b"llo"[..]);
```

While a frame is alive, the read buffer is not unique, and the frame keeps
the whole allocation alive, unless the frame was small enough to inline.
Once all shared frames are dropped, the read buffer is unique again, and a
later `clear()` or `reserve()` can reuse the space in front of its data.
If it must grow before then, it copies only its remaining data into another
buffer; existing frames remain valid in the old one.

## `ByteString`

[`ByteString`] is an immutable UTF-8 string backed by `Bytes`. It has the same
storage kinds and cost model, and dereferences to `&str`.

```rust
use ntex_bytes::{ByteString, Bytes};

let header = ByteString::from("content-type: text/plain");
let name = header.slice(0..12); // copies this short result inline on 64-bit targets
assert_eq!(name, "content-type");

// validates UTF-8, takes over the bytes without copying
let s = ByteString::try_from(Bytes::from_static(b"utf-8 text")).unwrap();
assert_eq!(s.as_str(), "utf-8 text");
```

Indices are byte offsets, not character positions. `slice`, `split_to` and
`split_off` panic if an index is not on a UTF-8 character boundary, so take
care when working with non-ASCII text. [`ByteString::from_bytes_unchecked`]
is unsafe: it skips validation, and the caller must guarantee valid UTF-8.
Use the checked conversion unless that guarantee is already established.
With the `simd` feature, UTF-8 validation uses SIMD instructions.

## `Buf` and `BufMut`

[`Buf`] reads from a buffer with a cursor, [`BufMut`] writes into one. They
provide the `get_*` and `put_*` helpers for integers and floats in big- and
little-endian order. The unsuffixed integer methods use big-endian order;
methods ending in `_le` use little-endian order.

```rust
use ntex_bytes::{Buf, BufMut, BytesMut};

let mut buf = BytesMut::with_capacity(16);
buf.put_u32(0xDEAD_BEEF);
buf.put_u8(1);

let mut b = buf.freeze();
assert_eq!(b.get_u32(), 0xDEAD_BEEF);
assert_eq!(b.get_u8(), 1);
assert!(!b.has_remaining());
```

Reading advances the cursor: after `get_u32()`, those four bytes are no
longer part of the remaining input. Reads panic if there are not enough bytes,
so a decoder should check `remaining()` or `len()` before reading a field.
The traits work with more than these owned buffer types: a `&[u8]` can be a
read cursor too, and a generic `Buf` need not store all its data contiguously.

[`Bytes`] and [`BytesMut`] also implement the `Buf` trait of the `bytes`
crate, and `BytesMut` its `BufMut` trait, so they work with libraries built
on `bytes`. `BytesMut` and `BytePages` implement `std::io::Write`, and
`BytesMut` implements `std::fmt::Write`, so `write!` can format into them.
`Bytes` and `ByteString` implement `serde`'s `Serialize` and `Deserialize`.

### Panicking and checked operations

Methods such as `slice`, `split_to`, `split_off`, `advance_to` and the
`get_*` family assume their indices or lengths have already been validated.
They panic on an invalid range or insufficient input. This keeps a codec's
hot path simple after it has performed its bounds checks.

For values derived directly from input, prefer the checked variants where
available: [`Bytes::slice_checked`], [`Bytes::split_to_checked`],
[`Bytes::split_off_checked`] and [`BytesMut::split_to_checked`]. A failed
check then becomes `None` instead of a process-level panic.

## Internal organization

You do not need the layout details to use the types, but they explain two
design choices: why tiny values are copied and why a mutable buffer can share
an allocation without sharing its writable bytes.

### Handles

`Bytes` is three machine words: a data pointer, a length, and an `offset`
word. The two low bits of `offset` select the storage kind, the remaining
bits hold the distance from the start of the heap buffer to the data
pointer. Inline storage reuses all three words for data and keeps the kind
and length in one byte, which is where the 23-byte inline capacity comes
from.

`BytesMut` is a single pointer to its heap buffer. Its view, the start and
length of its data, is stored in the buffer header, so `BytesMut` is one
word in size.

### Heap buffers

Each native shared heap buffer is one allocation: a 16-byte header followed
by the data. External storage and `Vec`-backed pages have their own layouts.

```text
          header (16 bytes)                       data
+--------+-----+----------+-----------+---------+---------+------------+-------+
| offset | len | capacity | ref_count | Bytes 1 | Bytes 2 |  BytesMut  | spare |
+--------+-----+----------+-----------+---------+---------+------------+-------+
^                                     ^                   ^            ^
allocation                            data start          offset       offset + len
```

| Field       | Meaning                                                                 |
|-------------|-------------------------------------------------------------------------|
| `offset`    | Start of the `BytesMut` view, from the beginning of the allocation      |
| `len`       | Length of the `BytesMut` view                                           |
| `capacity`  | Data capacity of the whole allocation, never changes while it is shared |
| `ref_count` | Number of handles: the `BytesMut` plus every `Bytes` view               |

Everything else is derived from these fields. The spare capacity after the
`BytesMut` view is `16 + capacity - offset - len`, and the
[`BytePageSize`] class follows from the allocation size `16 + capacity`,
see [Pooled buffers](#pooled-buffers).

Lengths, capacities and offsets use `u32`; the reference count is an
`AtomicU32`. A native heap buffer holds at most `u32::MAX - 16` bytes of data,
just under 4 GiB. Requests for larger capacities panic.

Each `Bytes` view stores its own pointer and length, and computes the header
address from its `offset`. Views split off a `BytesMut` cover memory in front
of the `BytesMut` view, so the `BytesMut` never writes to memory that a
`Bytes` can read.

### Reference counting and threads

The reference count is atomic and follows the same memory ordering as
`std::sync::Arc`: a `Release` decrement, and an `Acquire` fence before the
last handle frees the buffer. [`BytesMut::is_unique`] uses an `Acquire`
load, so after it returns `true`, all accesses through dropped handles,
including handles dropped on other threads, happen before the buffer is
reused. Apart from the reference count, `Bytes` handles read only the
`capacity` field of the header, which does not change while the buffer is
shared, so the `BytesMut` handle can update its view without
synchronization.

## Allocation strategy

### Regular buffers

[`BytesMut::with_capacity`] makes one allocation of `16 + capacity` bytes,
and [`BytesMut::capacity`] is exactly the requested capacity. Buffers
created this way, by `copy_from_slice`, and by conversions such as
`Bytes::from(Vec<u8>)` have no page
size, unless the requested capacity is exactly a page capacity. They are
freed when their last handle is dropped.

In particular, `Bytes::from(Vec<u8>)` copies the data into the crate's storage
(or inline for small values); it does not adopt the vector allocation.
The shared-buffer header needs space before the data. If you are building
data specifically to share as `Bytes`, starting with a `BytesMut` avoids
that conversion copy.

### Growth

Appending data reserves capacity as needed. `reserve(additional)` asks for
room for that many bytes **after the current data**, not for a total capacity.
[`BytesMut::reserve`] tries these options in order:

1. If the buffer has enough spare capacity, nothing happens.
2. If the buffer is unique and the whole allocation is large enough, the data
   is moved to the front of the allocation, reusing the space of dropped
   `Bytes` views.
3. If the buffer is unique and has no page size, the allocation is grown with
   `realloc`, which can often extend it in place.
4. If the buffer has a page size, its data moves to a pooled page, see
   [Pooled buffers](#pooled-buffers).
5. Otherwise, the data is copied into a new allocation without a page size.
   `Bytes` views split off the buffer keep the old allocation alive.

When growth needs more storage, the target capacity is at least twice the
current length, so repeatedly appending small amounts does not allocate on
every append. `realloc` may extend an allocation in place, but it may also
move it; the buffer makes no promise that its address stays the same.

The three reservation methods serve different purposes:

| Method | Use it when |
|--------|-------------|
| [`BytesMut::reserve`] | You want room for more bytes and expect the buffer to keep growing. |
| [`BytesMut::reserve_exact`] | You know how many more bytes you need and want to avoid the doubling policy. The allocation is exactly the new capacity plus the 16-byte header; pooled buffers are not rounded up to a page class and keep one only if the new capacity is a page capacity. |
| [`BytesMut::reserve_more`] | You don't know how much more is coming, for example the next read. If less than `BytePageSize::low` remains (1 KiB for `Size16`, 4 KiB without a page size), the allocation is reused when it holds the data plus `half_capacity()`: a unique buffer is compacted in place, a shared page moves to a page of the same class. Otherwise a pooled buffer moves to the next page class and any other buffer grows by its capacity, by at least 112 bytes and at most 64 KiB. |

Neither `reserve` nor `reserve_exact` is a general-purpose shrinking
operation: if there is already enough spare capacity, it leaves the buffer
alone.

### Page sizes

[`BytePageSize`] defines the size classes for pooled buffers. A page
allocation requests exactly the class size, including the header, rather
than a class-sized payload plus extra metadata. This avoids overshooting
those useful size boundaries, though an allocator's actual size classes
are its own implementation detail. Data capacity is the class size minus
the 16-byte header. These are allocation categories, not operating-system
virtual-memory pages.

| Class     | Allocation | Capacity      | `half_capacity()` | `low()`   | Cached pages by default |
|-----------|------------|---------------|-------------------|-----------|-------------------------|
| `Size4`   | 4 KiB      | 4,080 bytes   | 2 KiB             | 256 bytes | 64                      |
| `Size8`   | 8 KiB      | 8,176 bytes   | 4 KiB             | 512 bytes | 32                      |
| `Size16`  | 16 KiB     | 16,368 bytes  | 8 KiB             | 1 KiB     | 64                      |
| `Size24`  | 24 KiB     | 24,560 bytes  | 12 KiB            | 1.5 KiB   | 16                      |
| `Size32`  | 32 KiB     | 32,752 bytes  | 16 KiB            | 2 KiB     | 16                      |
| `Size48`  | 48 KiB     | 49,136 bytes  | 16 KiB            | 3 KiB     | 8                       |
| `Size64`  | 64 KiB     | 65,520 bytes  | 16 KiB            | 4 KiB     | 8                       |
| `Size128` | 128 KiB    | 131,056 bytes | 16 KiB            | 8 KiB     | 2                       |
| `Size256` | 256 KiB    | 262,128 bytes | 16 KiB            | 16 KiB    | 1                       |
| `Unset`   | -          | 65,520 bytes  | 16 KiB            | 4 KiB     | never cached            |

`Size16` is the default class. [`BytePageSize::for_capacity`] returns the
smallest class that holds a given capacity, or `Unset` above the largest
data capacity (262,128 bytes).
[`BytePageSize::next`] and [`BytePageSize::prev`] step between classes.
`half_capacity()` is the recommended write-buffer threshold for a page size,
`low()`, 2/32 of the class size, is the free-capacity threshold of
[`BytesMut::reserve_more`].
The enum is `#[non_exhaustive]`, so more classes may be added.

```rust
use ntex_bytes::BytePageSize;

assert_eq!(BytePageSize::for_capacity(100), BytePageSize::Size4);
assert_eq!(BytePageSize::for_capacity(20_000), BytePageSize::Size24);
assert_eq!(BytePageSize::Size16.next(), BytePageSize::Size24);
assert_eq!(BytePageSize::Size256.next(), BytePageSize::Unset);
```

### Pooled buffers

[`BytesMut::with_page_size`] takes a page of the given class from the
current thread's page cache, or allocates a new one if the cache is empty.
`with_page_size(BytePageSize::Unset)` takes a `Size64` page, the two have
the same allocation size. [`BytesMut::page_size`] reports the class of a
buffer.

The header does not store the class. A buffer belongs to a class when its
allocation, header included, is exactly the class size, so the class is
looked up from the `capacity` field. Any buffer of a page size is pooled,
including one created by `with_capacity` or `copy_from_slice` with exactly
a page capacity, or grown into one by `realloc`.

When the last handle to a page is dropped, whether it is the `BytesMut` or
a `Bytes` view split off it, the page returns to the cache of its class on
the thread that drops it. If that cache is full, the page is freed. Only
buffers with a page size are cached. Buffers with `Unset` are always freed.

A pooled buffer that grows moves to a page of the smallest class that fits
the new capacity, but never to a smaller class than its current one. The
data is copied, and the old page returns to the cache once its `Bytes`
views are dropped. If the growth target exceeds the largest page's data
capacity, the buffer becomes a regular buffer without a page size. From then
on it grows like any regular buffer and is freed when its last handle drops.

```rust
use ntex_bytes::{BytePageSize, BytesMut};

let mut buf = BytesMut::with_page_size(BytePageSize::Size4);
assert_eq!(buf.capacity(), 4096 - 16);

buf.extend_from_slice(&[0; 5000]);
assert_eq!(buf.page_size(), BytePageSize::Size8);

buf.extend_from_slice(&[0; 10_000]);
assert_eq!(buf.page_size(), BytePageSize::Size16);

// beyond the largest class, a regular buffer
buf.reserve(300 * 1024);
assert_eq!(buf.page_size(), BytePageSize::Unset);
```

Small results are inlined, which affects pooled buffers too: freezing a
pooled buffer with at most 23 bytes of data copies them inline. If no earlier
`Bytes` views still share the allocation, the page can return to the cache
right away.

```rust
use ntex_bytes::{BytePageSize, BytesMut};

let mut buf = BytesMut::with_page_size(BytePageSize::Size32);
buf.extend_from_slice(b"small");

let small = buf.freeze();
assert!(small.is_inline());
assert_eq!(small, b"small"[..]);
// `small` no longer needs the page; a later allocation can reuse it.
```

Pages handed to another thread return to that thread's cache. A thread that
only receives data, for example a thread that writes data produced on other
threads, fills its cache up to the limits and frees the rest. While a
thread's thread-local storage is being destroyed, the cache is unavailable,
and pages released then are freed.

### Tuning the page cache

Caching trades retained memory for fewer allocations. An idle worker can
still hold cached pages, but those pages are ready for reuse; they are not
live messages.

[`set_page_cache_size`] sets the number of cached pages of one class for the
current thread. The defaults, listed in the table above, cache more pages of
small classes and fewer of large ones. With all caches full, a thread retains
about 3.75 MiB.

```rust
use ntex_bytes::{BytePageSize, set_page_cache_size};

// retain more large pages for a thread that handles large messages
set_page_cache_size(BytePageSize::Size256, 4);

// stop returning 4 KiB pages to this thread's cache
set_page_cache_size(BytePageSize::Size4, 0);
```

The setting affects only the calling thread, so call it on every thread that
uses pooled buffers, for example at the start of each worker thread. A
smaller limit does not free pages already in the cache. They are reused, and
the limit applies when pages are released. The older `set_pages_cache()`,
which sets one limit for all classes, is deprecated.

These limits count **cached** pages, not pages in use. They do not bound the
memory held by live requests or slow consumers. Start with the defaults, then
adjust them using the sizes and concurrency of your actual workload. More
workers also means more independent caches.

For example, the default cache can retain roughly 3.75 MiB per worker when
every size class is full. Eight otherwise idle workers could therefore retain
about 30 MiB in page caches. That may be a good trade when traffic returns
quickly; for sparse workloads or many workers, smaller limits may be better.

## `BytePages`

Suppose a response contains a short header, a large existing body and a
trailer. A contiguous buffer may have to move earlier output as it grows,
and copying the body into it would be wasteful. [`BytePages`] keeps a queue
of chunks instead: new bytes fill a current writable page, while owned
payloads can be queued separately. When the current page fills, another
page is started. Existing output stays where it is.

```rust
use ntex_bytes::{BufMut, BytePageSize, BytePages, Bytes};

let mut pages = BytePages::new(BytePageSize::Size4);
pages.extend_from_slice(b"HTTP/1.1 200 OK\r\n\r\n");
let body = Bytes::copy_from_slice(&[b'x'; 8192]);
pages.append(body); // moves the existing body into the queue, no extra copy
pages.put_slice(b"trailer");
assert_eq!(pages.num_pages(), 3);

// the transport consumes pages in order
while let Some(page) = pages.take() {
    // write `page` to the socket
    assert!(!page.is_empty());
}
```

How data enters the queue:

- `put_slice()`, `extend_from_slice()` and the other `BufMut` methods copy
  data into the current page. New pages are taken from the page cache with
  the queue's page size.
- [`BytePages::append`] adds an owned buffer. If the current page holds
  data, buffers of up to 4 KiB are copied into it. Larger buffers become a
  separate page without copying, and the current page keeps its spare
  capacity for later writes. If the current page is empty, the buffer is
  added without copying, and a unique `BytesMut` with spare capacity
  becomes the new current page.
- [`BytePages::prepend`] inserts a page at the front.

How data leaves it:

- [`BytePages::take`] returns the next [`BytePage`], with the current page
  last.
- [`BytePages::split_to`] and [`BytePages::split_into`] move a byte prefix to
  another queue, splitting a page if needed.
- [`BytePages::freeze`] returns all data as one `Bytes`. A single page is
  converted according to its storage kind: native heap storage can be
  reused, tiny data may be inlined, and `Vec` storage is copied. Several
  pages are copied into one buffer.
- [`BytePages::copy_to`] leaves the source unchanged and appends cloned
  pages to another queue. [`BytePages::move_to`] leaves the source empty
  and appends its pages instead. Both follow `append`'s small-buffer copy
  rules; `copy_to` also copies `Vec`-backed pages when cloning them.

A [`BytePage`] holds a `Bytes`, the storage of a `BytesMut`, or a `Vec<u8>`.
Splitting or advancing a `Vec` page copies its data, so prefer the other
kinds when output will be split or shared. Queuing an owned `Vec` directly
can still avoid an initial copy; that is different from converting the
vector to `Bytes` first.

Keep data paged for as long as the consumer accepts chunks. Calling
`freeze()` just before passing it to a chunk-aware transport would undo the
main benefit by combining all pages into one allocation.

The page size of a `BytePages` cannot be `Unset`, [`BytePages::new`] and
[`BytePages::set_page_size`] panic on it. `BytePages::default()` uses
`Size16`. The page list itself is also reused: each thread keeps up to 128
empty page lists for new `BytePages` values.

Changing the page size affects future page allocations, not data already
queued or the current page. Page buffers are allocated lazily, so creating
an empty queue does not immediately allocate a payload page.

## Buffers in the I/O layer

The [I/O Abstraction Layer](./7-io.md) builds on these types, reads and writes
share the same per-thread page cache:

- **Reads.** The transport reads into a pooled `BytesMut` read buffer. Codecs
  split frames off it with `split_to`, so large decoded messages share the
  read buffer while tiny ones are copied inline. New read buffers use a
  per-connection page size that adapts to the read load between the min and
  max read sizes of [`IoConfig`], `Size4` and `Size64` by default. A buffer
  grows once less than `BytePageSize::low()` of its page remains free, larger
  input moves it to bigger page sizes, beyond `Size256` it becomes an
  unpooled buffer that is freed when empty.
- **Writes.** Encoders write into `BytePages` with the page size of
  [`IoConfig::write_size`], `Size16` by default. `IoRef::encode_bytes`
  appends owned buffers, so large payloads are not copied. The write
  threshold, `half_capacity()` of the page size by default, controls when a
  transport may start writing while output is still being produced.

An empty read buffer is released immediately, but its page returns to the
cache only when the last frame split from it is dropped. Holding a whole
request or frame therefore keeps the full page alive; copy the field instead
when only a small part needs to survive. [`set_page_cache_size`] tunes the
cache for read and write pages alike.

### What memory usage means in practice

When investigating memory growth, separate three categories:

- **Live data:** bytes still owned by requests, responses, transports or
  application state.
- **Retained allocations:** a small live view keeps a larger shared allocation
  alive. `trimdown` can help when the view is intentionally long-lived.
- **Cached allocations:** empty pages or read buffers kept for reuse. Cache
  limits control this category, not live data.

Only the first category is directly proportional to pending work. The other
two are performance tradeoffs and can make resident memory stay high after a
traffic spike without indicating an ownership leak.

## Performance guidelines

The fastest choice depends on how long the data lives, not just how large
it is. A useful starting point is:

| Situation | Start with |
|-----------|------------|
| Hand a decoded frame to its immediate consumer | `split_to` or `take`, so large payloads can share the input allocation. |
| Keep one field in a long-lived cache | A slice followed by `trimdown`, or an explicit copy, to avoid retaining an entire input buffer. |
| Build a contiguous message of a known size | `BytesMut::with_capacity`, or one `reserve` for the bytes still to be written. |
| Repeatedly build short-lived I/O buffers | `with_page_size` with a class that fits the usual size; cache hits avoid a fresh payload allocation. |
| Send an existing large body | `BytePages::append`, rather than copying it with `put_slice`. |
| Use constant protocol bytes | `from_static`, which neither allocates nor copies the bytes. |

Drop shared frames when they are no longer needed, and do not equate
"cheap to clone" with "cheap to retain". A clone may cost only an atomic
increment yet extend the lifetime of a large allocation. Conversely, an
inline clone copies a few bytes but needs neither allocation nor reference
counting. Measure allocation volume and retained memory alongside throughput
when deciding whether to share, copy or cache.

[`ntex::util`]: https://docs.rs/ntex/latest/ntex/util/index.html
[`Buf`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/buf/trait.Buf.html
[`BufMut`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/buf/trait.BufMut.html
[`BytePage`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePage.html
[`BytePageSize`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/enum.BytePageSize.html
[`BytePageSize::for_capacity`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/enum.BytePageSize.html#method.for_capacity
[`BytePageSize::next`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/enum.BytePageSize.html#method.next
[`BytePageSize::prev`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/enum.BytePageSize.html#method.prev
[`BytePages`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html
[`BytePages::append`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.append
[`BytePages::copy_to`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.copy_to
[`BytePages::freeze`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.freeze
[`BytePages::move_to`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.move_to
[`BytePages::new`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.new
[`BytePages::prepend`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.prepend
[`BytePages::set_page_size`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.set_page_size
[`BytePages::split_into`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.split_into
[`BytePages::split_to`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.split_to
[`BytePages::take`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytePages.html#method.take
[`ByteString`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.ByteString.html
[`ByteString::from_bytes_unchecked`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.ByteString.html#method.from_bytes_unchecked
[`Bytes`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html
[`Bytes::from_ext`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html#method.from_ext
[`Bytes::from_static`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html#method.from_static
[`Bytes::is_inline`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html#method.is_inline
[`Bytes::slice`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html#method.slice
[`Bytes::slice_checked`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html#method.slice_checked
[`Bytes::split_off_checked`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html#method.split_off_checked
[`Bytes::split_to`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html#method.split_to
[`Bytes::split_to_checked`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html#method.split_to_checked
[`Bytes::trimdown`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.Bytes.html#method.trimdown
[`BytesMut`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html
[`BytesMut::advance_to`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.advance_to
[`BytesMut::capacity`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.capacity
[`BytesMut::clear`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.clear
[`BytesMut::freeze`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.freeze
[`BytesMut::is_unique`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.is_unique
[`BytesMut::new`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.new
[`BytesMut::page_size`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.page_size
[`BytesMut::reserve`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.reserve
[`BytesMut::reserve_exact`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.reserve_exact
[`BytesMut::reserve_more`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.reserve_more
[`BytesMut::split_to`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.split_to
[`BytesMut::split_to_checked`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.split_to_checked
[`BytesMut::take`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.take
[`BytesMut::with_capacity`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.with_capacity
[`BytesMut::with_page_size`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/struct.BytesMut.html#method.with_page_size
[`IoConfig`]: https://docs.rs/ntex/latest/ntex/io/struct.IoConfig.html
[`IoConfig::write_size`]: https://docs.rs/ntex/latest/ntex/io/struct.IoConfig.html#method.write_size
[`set_page_cache_size`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/fn.set_page_cache_size.html
[`StorageExt`]: https://docs.rs/ntex-bytes/latest/ntex_bytes/trait.StorageExt.html
