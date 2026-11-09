# Byte Buffers

Use `BytesMut` to build a buffer and `Bytes` to hand its data to other code.
For example, a decoder may receive two messages in one socket read. It can
hand the first message to your application and keep the second for later.
The application gets an owned view that keeps the data alive without borrowing
the decoder's read buffer.

Copying each message into a new `Vec<u8>` would add allocations and memory
copies to every request. On the write side, you can queue a small header and
a large body separately instead of copying them into one growing buffer.

Sharing costs more than copying for tiny values, so `ntex-bytes` stores short
byte strings directly in their handles. Per-thread page caches reduce trips
to the allocator for frequently reused I/O buffers.

The `ntex` crate re-exports the buffer types in [`ntex::util`]. These include
[`Bytes`] and [`BytesMut`], along with [`ByteString`] for text. For chunked
output, it provides [`BytePages`], [`BytePage`] and [`BytePageSize`]. The
[`Buf`] and [`BufMut`] traits provide read and write helpers.
[`set_page_cache_size`] is available directly from `ntex_bytes` for cache
tuning.

## The mental model

Use `BytesMut` while filling or changing a contiguous buffer. Switch to
`Bytes` when the data is ready to share. Use `BytePages` when output can stay
in separate chunks. `ByteString` is the text version of `Bytes` and
guarantees valid UTF-8.

A **handle** and its **allocation** have separate lifetimes. Several handles
can keep one allocation alive, but they do not all have permission to change
it. A `BytesMut` owns the writable tail. `Bytes` views can own earlier,
read-only portions of the same allocation.

Splitting off a message gives it an independent lifetime, though it may still
share storage. Share data while it moves through the pipeline. Copy or trim
a small part when it needs to live much longer than its original buffer.

Here, "zero-copy" means avoiding a copy between application buffers. It does
not promise that the operating system or a TLS layer will avoid copying.
Copying a handful of bytes is often cheaper than maintaining another shared
reference.

## Types at a glance

- [`Bytes`] is immutable. Use it for decoded frames, payloads and header
  values. Cloning usually shares storage or copies inline data; external
  storage decides how it is cloned.
- [`BytesMut`] has unique write access. Use it for read buffers and building
  output. Cloning copies the data.
- [`ByteString`] is immutable UTF-8 text, useful for header names, paths and
  topics. Cloning has the same cost as `Bytes`.
- [`BytePages`] has unique write access. Use it for write queues. Cloning
  shares or copies data according to page storage and append rules.
- [`BytePage`] is one immutable chunk of a `BytePages` queue. Cloning shares
  data, except for `Vec` pages, which copy it.
- [`BytePageSize`] defines allocation size classes.

## `Bytes`

Use [`Bytes`] when you need an immutable, owned view of contiguous data.
Heap-backed values normally share a reference-counted allocation when you
clone, slice or split them.

`Bytes` is `Send + Sync`, so you can pass it between threads. Small values
and static data use simpler storage, described below.

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

The cost of cloning depends on which of four storage kinds the value uses.
Most large values use a shared heap buffer, usually from [`BytesMut`], so a
clone only increments its reference count.

Small values use inline storage instead: at most 23 bytes, or 11 bytes on
32-bit targets, fit inside the handle. Cloning copies those bytes without
allocating. Static data, created by [`Bytes::from_static`],
`From<&'static [u8]>` or `From<&'static str>`, needs only a pointer copy.

The fourth kind is external storage, created by [`Bytes::from_ext`] or from
an `Arc<str>` through [`ByteString`]. Its owner's vtable handles cloning;
[Externally owned data](#externally-owned-data) explains that integration.

Inline storage also changes the cost of taking a small view.
[`Bytes::slice`] copies a small result inline instead of taking another
reference to the heap buffer. [`Bytes::split_to`] does the same, as do
[`BytesMut::split_to`] and [`BytesMut::freeze`]. A short header name can
outlive a request without keeping its read buffer alive.

Use [`Bytes::is_inline`] to check the storage kind. Static constructors keep
their static representation even for short strings.

The examples below assume a 64-bit target, where the inline limit is 23 bytes.
On 32-bit targets it is 11 bytes. These limits and the layouts later in the
chapter describe the current implementation, not a stable memory-layout API.
Unless stated otherwise, allocation capacities and header sizes assume the
`tracker` feature is disabled.
[Tracking buffer memory](#tracking-buffer-memory) explains how enabling it
enlarges the header.

### When sharing retains too much memory

When you keep a small part of a large buffer for a long time, consider
[`Bytes::trimdown`]. A 100-byte token from a 16 KiB read buffer can keep the
whole allocation alive. The allocation is still owned, so this retention is
not a leak.

A view that is too large to inline keeps the allocation alive even if it
uses only a small part. `trimdown` moves data inline when it fits. Otherwise,
it copies the data into an exactly sized buffer if at least 64 bytes of the
backing capacity are unused. It leaves inline and static values alone.

```rust
use ntex_bytes::Bytes;

let packet = Bytes::copy_from_slice(&[b'x'; 16 * 1024]);
let mut token = packet.slice(100..200);
token.trimdown(); // copy 100 bytes so `token` no longer retains the packet
drop(packet);
assert_eq!(token.len(), 100);
```

Trim long-lived values where the retained memory matters. Trimming every
frame loses the benefit of sharing. Trimming one view also cannot release
the original allocation while other handles still own it.

### Externally owned data

Use external storage to share memory that another type owns. For example,
`ByteString::from(Arc<str>)` shares the string without copying it. A normal
decoder does not need to implement this integration API.

The owner implements the unsafe [`StorageExt`] trait. It supplies a static
vtable with `as_ptr`, `len`, `clone` and `drop` functions. If `clone` returns
`None`, clones copy the data into native storage instead. `Arc<str>`
implements this trait.

The unsafe contract requires the exposed bytes to stay valid and immutable
for every live handle, including across threads. The vtable must clone and
release ownership correctly.

## `BytesMut`

Use [`BytesMut`] to build output or receive data into a growable buffer.
Like `Vec<u8>`, it gives you unique write access to its data. You can split
completed data off the front as `Bytes` while continuing to fill the tail.

The handles can share a heap allocation, but only `BytesMut` can write into
its part. In a decoder, that allocation might look like this:

```text
| completed frame | completed frame | unread data | spare capacity |
|<------ immutable Bytes views ---->|<--------- BytesMut -------->|
```

Only one `BytesMut` can own the writable part of an allocation. When you
hand out a prefix as `Bytes`, the mutable view advances past it and cannot
overwrite it. The decoder can fill the tail while the application reads
earlier frames, without locking the data.

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

Choose how to hand off data based on what should happen to the mutable
handle. After decoding a complete prefix, use [`BytesMut::split_to`]. It
returns the first `at` bytes as `Bytes`, leaving the remainder for further
reads. When all current bytes form one message, [`BytesMut::take`] returns
them and keeps the spare capacity for `self`. In either case, the
connection can keep using the mutable handle.

When you are finished with that handle, use [`BytesMut::freeze`] instead.
It consumes the buffer and returns `Bytes`. None of these operations is
always faster: small results are copied inline, while larger results
usually share the allocation. Choose the ownership behavior you need,
then measure if that path matters.

To discard input rather than hand it off, [`BytesMut::advance_to`] drops
the first `cnt` bytes in `O(1)` time. [`BytesMut::clear`] empties the view
and reclaims its full capacity if it is unique. Use [`BytesMut::reserve`]
when you need more spare space; [Growth](#growth) explains how it reuses
or replaces the allocation.

[`BytesMut::is_unique`] tells you whether any `Bytes` still shares the
allocation. The same distinction matters when converting back from
`Bytes`: `BytesMut::from(Bytes)` reuses a unique shared heap allocation,
but copies inline, static, external or still-shared data.

[`BytesMut::new`] currently starts with 112 bytes of data capacity. Including
the header, its allocation is 128 bytes. [`BytesMut::with_capacity`]
allocates exactly the requested capacity. [`BytesMut::with_page_size`] takes
a pooled page; see [Pooled buffers](#pooled-buffers).

Cloning a `BytesMut` always copies the data.

### Length, capacity and spare space

Use `len()` to find how many bytes are initialized. `capacity()` tells you
how much the current mutable view can hold, including those bytes. The
difference is spare space at the end.

Capacity can be smaller than the whole allocation. Splitting or advancing
the front shrinks the mutable view. Dropping a frame does not immediately
move the read cursor backward.

A later `clear()` reclaims the whole allocation if it is unique. When more
space is needed, `reserve()` can move unread data to the front.

Use `clear()` to discard data while keeping the allocation for another use.
Drop the buffer when you no longer need it. Its memory is then freed or
cached, depending on how it was allocated.

### A buffer's typical journey

These operations let a decoder reuse one allocation for many messages.
A typical cycle looks like this:

1. The transport writes socket data into the spare tail of a `BytesMut`.
2. The decoder examines the initialized prefix.
3. A complete frame is removed with `split_to`.
4. The returned `Bytes` travels through services and application code.
5. The `BytesMut` stays with the connection and receives more input.
6. When all shared frames are dropped, `reserve` or `clear` can reclaim the
   space at the front.

Frames that share storage keep the whole allocation alive and prevent reuse
of their bytes. Tiny inline frames do neither. The mutable tail stays
writable while earlier frames are alive.

If the tail fills before those frames are dropped, growth copies only the
remaining data into a new allocation. Existing frames stay valid in the old
one.

`BytesMut` implements the ntex [`BufMut`] trait and the `bytes` crate's
`BufMut` trait. Their `BufMut::remaining_mut()` methods differ. The ntex
version reports current spare capacity. The `bytes` version reports
`usize::MAX - len` because the buffer grows on demand.

All `put_*` methods reserve capacity as needed in both cases.

### Zero-copy decoding

In an ntex codec, split completed frames off the read buffer to hand them
to the application. Large frames share the read buffer; tiny ones are copied
inline. This decoder reads a two-byte, big-endian length followed by a
payload:

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

The decoder treats the length field as untrusted input. It checks for a
complete frame before calling `advance_to`. The same check makes the later
`split_to` safe. Both methods panic if asked to move past the end.

Use [`BytesMut::split_to_checked`] if you have not already validated the
index:

```rust
use ntex_bytes::{Bytes, BytesMut};

fn take_prefix(src: &mut BytesMut, len: usize) -> Option<Bytes> {
    src.split_to_checked(len)
}

let mut src = BytesMut::copy_from_slice(b"hello");
assert_eq!(take_prefix(&mut src, 2).unwrap(), b"he"[..]);
assert_eq!(src, b"llo"[..]);
```

Bounds checks do not impose a size limit. A real decoder should reject a
declared length above its protocol or application limit before calling
`reserve`. Otherwise, a peer can make the connection retain a very large
buffer without ever sending the promised frame.

## `ByteString`

When a decoded field is text, [`ByteString`] lets you keep the sharing
behavior of `Bytes` while guaranteeing valid UTF-8. It is backed by `Bytes`
and dereferences to `&str`, with the same storage kinds and cloning costs.

```rust
use ntex_bytes::{ByteString, Bytes};

let header = ByteString::from("content-type: text/plain");
let name = header.slice(0..12); // copies this short result inline on 64-bit targets
assert_eq!(name, "content-type");

// validates UTF-8, takes over the bytes without copying
let s = ByteString::try_from(Bytes::from_static(b"utf-8 text")).unwrap();
assert_eq!(s.as_str(), "utf-8 text");
```

Convert an owned `Bytes` or `BytesMut` into `ByteString` to validate UTF-8
and reuse the bytes. The usual small-value inlining still applies.
Converting back with [`ByteString::into_bytes`] is free.

A `String` or `Vec<u8>` conversion copies into native byte storage.
`ByteString::from(Arc<str>)` shares the `Arc` instead.

```rust
use ntex_bytes::{ByteString, BytesMut};

let bytes = BytesMut::copy_from_slice("hello".as_bytes()).freeze();
let text = ByteString::try_from(bytes).unwrap();
assert_eq!(text, "hello");

let bytes = text.into_bytes();
assert_eq!(bytes, b"hello"[..]);
```

Indices are byte offsets, not character positions. `slice` panics if an
index is not on a UTF-8 character boundary. So do `split_to` and `split_off`.
Take care when working with non-ASCII text.

[`ByteString::from_bytes_unchecked`] skips validation and is unsafe. You must
guarantee valid UTF-8 when calling it. Use the checked conversion unless you
have already established that guarantee. With the `simd` feature, UTF-8
validation uses SIMD instructions.

Like `Bytes`, a `ByteString` can keep a larger allocation alive. Call
[`ByteString::trimdown`] when moving a short string from a request buffer
into long-lived state.

## `Buf` and `BufMut`

Use [`Buf`] to read values from a buffer with a cursor. Use [`BufMut`] to
write them. The traits provide `get_*` and `put_*` helpers for integers and
floats in big- and little-endian order.

Unsuffixed integer methods use big-endian order. Methods ending in `_le`
use little-endian order.

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

Reading advances the cursor. After `get_u32()`, those four bytes are no
longer part of the remaining input. Reads panic if there are not enough
bytes, so check `remaining()` or `len()` before reading a field.

The traits also work with other buffer types. A `&[u8]` can serve as a read
cursor, and a generic `Buf` need not store all its data contiguously.

[`Bytes`] and [`BytesMut`] implement the `bytes` crate's `Buf` trait.
`BytesMut` also implements that crate's `BufMut` trait. You can use these
types with libraries built on `bytes`.

For formatting with `write!`, `BytesMut` and `BytePages` implement
`std::io::Write`. `BytesMut` also implements `std::fmt::Write`. `Bytes` and
`ByteString` implement `serde`'s `Serialize` and `Deserialize`.

### Panicking and checked operations

Prefer checked methods for indices and lengths taken directly from input.
A failed check returns `None` rather than panicking. Checked variants include:

- [`Bytes::slice_checked`]
- [`Bytes::split_to_checked`]
- [`Bytes::split_off_checked`]
- [`BytesMut::split_to_checked`]

Methods such as `slice` assume you have already validated the range or
length. The same applies to `split_to` and `split_off`, as well as
`advance_to` and the `get_*` methods. They panic on an invalid range or
insufficient input. After a codec has checked its bounds, these methods keep
its hot path simple.

## Internal organization

You can use the buffer types without knowing their layouts. These details
explain the cost of copying tiny values and how a mutable buffer shares an
allocation while keeping its writable bytes private.

### Handles

The handles stay small even when they refer to large buffers. `Bytes` takes
three machine words, and `BytesMut` takes one.

A `Bytes` handle holds a data pointer, a length and an `offset` word. The two
low bits of `offset` select the storage kind. The remaining bits hold the
distance from the heap buffer's start to the data pointer.

Inline storage reuses all three words for data. It keeps the kind and length
in one byte, leaving room for 23 bytes of data on a 64-bit target.

A `BytesMut` handle is a single pointer to its heap buffer. The buffer header
stores the start and length of its data view.

### Heap buffers

Sharing a buffer does not require a separate allocation for its metadata.
A native shared heap buffer puts a 16-byte header directly before the data.
External storage and `Vec`-backed pages have their own layouts.

```text
          header (16 bytes)                       data
+--------+-----+----------+-----------+---------+---------+------------+-------+
| offset | len | capacity | ref_count | Bytes 1 | Bytes 2 |  BytesMut  | spare |
+--------+-----+----------+-----------+---------+---------+------------+-------+
^                                     ^                   ^            ^
allocation                            data start          offset       offset + len
```

The header holds four fields:

- `offset`: the start of the `BytesMut` view, measured from the beginning of
  the allocation.
- `len`: the length of the `BytesMut` view.
- `capacity`: the data capacity of the whole allocation. It never changes
  while the buffer is shared.
- `ref_count`: the number of handles, including the `BytesMut` and every
  `Bytes` view.

The other properties follow from these fields. Spare capacity after the
`BytesMut` view is `16 + capacity - offset - len`. The allocation size,
`16 + capacity`, determines the [`BytePageSize`] class; see
[Pooled buffers](#pooled-buffers).

Lengths, capacities and offsets use `u32`. The reference count is an
`AtomicU32`. A native heap buffer holds at most `u32::MAX - 16` bytes of data,
just under 4 GiB. Requests for larger capacities panic.

Each `Bytes` view stores its own pointer and length. It computes the header
address from its `offset`. Views split off a `BytesMut` cover memory in
front of the mutable view. The `BytesMut` never writes to memory that a
`Bytes` can read.

### Reference counting and threads

You can reuse a unique buffer even if its other handles were dropped on
another thread. [`BytesMut::is_unique`] uses an `Acquire` load. When it
returns `true`, all accesses through dropped handles happen before the
buffer is reused.

The atomic reference count follows the same memory ordering as
`std::sync::Arc`. Decrements use `Release`, and an `Acquire` fence runs
before the last handle frees the buffer.

Apart from the reference count, `Bytes` handles read only the header's
`capacity` field. That field does not change while the buffer is shared.
The `BytesMut` handle can therefore update its view without synchronization.

## Allocation strategy

Use regular buffers when you want a specific capacity. For repeatedly reused
I/O buffers, pooled pages can avoid fresh allocations. Appending data grows
either kind of buffer as needed.

### Regular buffers

Use [`BytesMut::with_capacity`] when you know how much space you need.
[`BytesMut::capacity`] is exactly the requested capacity. The buffer uses
one allocation of `16 + capacity` bytes.

If you are building data to share as `Bytes`, start with a `BytesMut` to
avoid a conversion copy. `Bytes::from(Vec<u8>)` copies into the crate's
storage, or inline for small values. It cannot adopt the vector allocation
because the shared-buffer header needs space before the data.

Buffers made with `with_capacity` have no page size unless the requested
capacity is exactly a page capacity. The same applies to `copy_from_slice`
and conversions such as `Bytes::from(Vec<u8>)`. Regular buffers are freed
when their last handle is dropped.

### Growth

Call `reserve(additional)` when you need room for more data. The argument
counts bytes **after the current data**, rather than a total capacity.
Appending data also reserves space as needed.

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
current length. Repeated small appends therefore do not allocate each time.
`realloc` may extend an allocation in place or move it. The buffer makes no
promise that its address stays the same.

The usual [`BytesMut::reserve`] is a good fit when you expect the buffer
to keep growing. If you know exactly how many more bytes you will write,
use [`BytesMut::reserve_exact`] to avoid the doubling policy. When it
allocates, the size is exactly the new capacity plus the 16-byte header.
It does not round pooled buffers up to a page class; they keep a class only
if the new capacity is a page capacity.

Before a socket read, you may not know how much data is coming. Use
[`BytesMut::reserve_more`] to grow by one step when spare capacity is low.
It acts when less than `BytePageSize::low` remains. That
threshold is 1 KiB for `Size16`, or 4 KiB without a page size. It reuses the
allocation when it can hold the data plus `half_capacity()`. A unique buffer
is compacted in place; a shared page moves to a page of the same class.

Otherwise, a pooled buffer moves to the next page class. Any other buffer
grows by its capacity, with growth of at least 112 bytes and at most 64 KiB.

Do not use `reserve` or `reserve_exact` as general-purpose shrinking
operations. They leave the buffer alone when it already has enough spare
capacity.

### Page sizes

Choose a [`BytePageSize`] to set the size class of a pooled buffer. Data
capacity is the class size minus the 16-byte header. These classes describe
allocations; they are unrelated to operating-system virtual-memory pages.

A page allocation requests exactly the class size, including its header.
It does not add metadata on top of a class-sized payload, which would
overshoot the size boundary. The allocator's own size classes remain an
implementation detail.

| Class     | Allocation | Capacity      | `half_capacity()` | `low()`   | Cached pages by default |
|-----------|------------|---------------|-------------------|-----------|-------------------------|
| `Size4`   | 4 KiB      | 4,080 bytes   | 2 KiB             | 256 bytes | 128                     |
| `Size8`   | 8 KiB      | 8,176 bytes   | 4 KiB             | 512 bytes | 64                      |
| `Size16`  | 16 KiB     | 16,368 bytes  | 8 KiB             | 1 KiB     | 64                      |
| `Size24`  | 24 KiB     | 24,560 bytes  | 12 KiB            | 1.5 KiB   | 32                      |
| `Size32`  | 32 KiB     | 32,752 bytes  | 16 KiB            | 2 KiB     | 16                      |
| `Size48`  | 48 KiB     | 49,136 bytes  | 16 KiB            | 3 KiB     | 8                       |
| `Size64`  | 64 KiB     | 65,520 bytes  | 16 KiB            | 4 KiB     | 16                      |
| `Size128` | 128 KiB    | 131,056 bytes | 16 KiB            | 8 KiB     | 2                       |
| `Size256` | 256 KiB    | 262,128 bytes | 16 KiB            | 16 KiB    | 1                       |
| `Unset`   | -          | 65,520 bytes  | 16 KiB            | 4 KiB     | never cached            |

`Size16` is the default class. Use [`BytePageSize::for_capacity`] to find
the smallest class that holds a given capacity. It returns `Unset` above
the largest data capacity, 262,128 bytes.

[`BytePageSize::next`] steps to the next class. [`BytePageSize::prev`]
steps to the previous one. The enum is `#[non_exhaustive]`, so more classes
may be added.

```rust
use ntex_bytes::BytePageSize;

assert_eq!(BytePageSize::for_capacity(100), BytePageSize::Size4);
assert_eq!(BytePageSize::for_capacity(20_000), BytePageSize::Size24);
assert_eq!(BytePageSize::Size16.next(), BytePageSize::Size24);
assert_eq!(BytePageSize::Size256.next(), BytePageSize::Unset);
```

`half_capacity()` is the recommended write-buffer threshold for a page
size. `low()` is 2/32 of the class size and sets the free-capacity threshold
for [`BytesMut::reserve_more`].

### Pooled buffers

Use [`BytesMut::with_page_size`] when buffers are created and released
repeatedly, so their pages can be reused. It first looks in the current
thread's cache for the requested class. If none is cached, it tries the
next smaller class before allocating a new page of the requested size.
Use [`BytesMut::page_size`] to check the class you actually received.
Growth uses the required class directly, without this fallback.

`with_page_size(BytePageSize::Unset)` requests a `Size64` page because the
two have the same allocation size. The same cache fallback applies.

When a pooled buffer grows, its data moves to a page of the smallest class
that fits the new capacity. The new class is never smaller than the current
one. Growth copies the data; the old page can return to the cache once its
`Bytes` views are dropped.

If the growth target exceeds the largest page's data capacity, the buffer
becomes a regular buffer without a page size. It then grows like any regular
buffer and is freed when its last handle drops.

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

Freezing a pooled buffer with at most 23 bytes of data on a 64-bit target
copies the result inline. If no earlier `Bytes` views still share the
allocation, the page can return to the cache right away.

```rust
use ntex_bytes::{BytePageSize, BytesMut};

let mut buf = BytesMut::with_page_size(BytePageSize::Size32);
buf.extend_from_slice(b"small");

let small = buf.freeze();
assert!(small.is_inline());
assert_eq!(small, b"small"[..]);
// `small` no longer needs the page; a later allocation can reuse it.
```

The header does not store a page class. The class is looked up from the
`capacity` field. A buffer belongs to a class when its allocation, including
the header, is exactly the class size.

Any buffer whose allocation matches a page class is pooled. This includes
buffers created by `with_capacity` or `copy_from_slice` with exactly a page
capacity. A buffer grown to that size by `realloc` is pooled too.

The last handle may be the `BytesMut` or a `Bytes` view split from it. When
that handle is dropped, the page returns to its class's cache on the thread
that drops it. If the cache is full, the page is freed. Only buffers with
a page size are cached; buffers with `Unset` are always freed.

Pages handed to another thread return to that thread's cache. A thread that
only receives data fills its cache up to the limits and frees the rest.
For example, this happens on a thread that writes data produced on other
threads.

The cache is unavailable while a thread's thread-local storage is being
destroyed. Pages released during that time are freed.

### Tuning the page cache

Start with the default cache limits, then adjust them for your workload's
buffer sizes and concurrency. Caching retains memory to reduce allocations.
An idle worker can hold cached pages ready for reuse, even though no live
messages need them.

[`set_page_cache_size`] sets the number of cached pages of one class for the
current thread. With all caches full, a thread retains about 5 MiB.

The defaults in the table above favor the classes I/O uses most. They
retain more 4 to 16 KiB pages, where connection reads start and which
include the default write page, as well as 64 KiB pages for busy
connections. Larger classes retain fewer pages.

```rust
use ntex_bytes::{BytePageSize, set_page_cache_size};

// retain more large pages for a thread that handles large messages
set_page_cache_size(BytePageSize::Size256, 4);

// stop returning 4 KiB pages to this thread's cache
set_page_cache_size(BytePageSize::Size4, 0);
```

The setting affects only the calling thread. Call it on every thread that
uses pooled buffers, for example when each worker thread starts.

Lowering a limit immediately frees cached pages above the new limit. It
also limits how many released pages can return to the cache. The older
`set_pages_cache()`, which sets one limit for all classes, is deprecated.

These limits count **cached** pages, not pages in use. They do not bound the
memory held by live requests or slow consumers. Each worker has its own
cache, so retention scales with the worker count.

## `BytePages`

Use [`BytePages`] when your output can stay in separate chunks. You can
write new bytes into its current page and queue owned payloads separately.
When the current page fills, the queue starts another one without moving
earlier output.

For example, a response might contain a short header, a large existing body
and a trailer. A contiguous buffer may move earlier output as it grows.
Queuing the body separately also avoids copying it into that buffer.

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

Build small pieces such as headers with `put_slice()`,
`extend_from_slice()`, or another `BufMut` method. They copy bytes into the
current page and take new pages from the page cache as needed, requesting
the queue's page size. For an owned payload, use [`BytePages::append`] instead.
Use [`BytePages::prepend`] when a page must go at the front.

A transport can drain the queue with [`BytePages::take`], which returns
the next [`BytePage`] and leaves the current page until last. To pass only
a byte prefix to another queue, use [`BytePages::split_to`] or
[`BytePages::split_into`]; they split a page if needed. To get all data as
one `Bytes`, use [`BytePages::freeze`], with the copying costs described
below.

`append` decides whether to copy based on the current page and buffer size.
If the current page holds data, buffers of up to 4 KiB are copied into it.
Larger buffers become separate pages without copying. The current page
keeps its spare capacity for later writes.

If the current page is empty, `append` adds the buffer without copying.
A unique `BytesMut` with spare capacity becomes the new current page.

To pass output to another queue without draining it yourself,
[`BytePages::copy_to`] appends cloned pages and leaves the source unchanged.
It follows `append`'s small-buffer copy rules and copies `Vec`-backed pages
when cloning them. [`BytePages::move_to`] instead transfers the pages as
they are, without copying or merging, and leaves the source empty.

With `freeze`, a single page is converted according to its storage kind.
Native heap storage can be reused, tiny data may be inlined, and `Vec`
storage is copied. If the queue has several pages, they are copied into
one buffer.

A [`BytePage`] holds a `Bytes`, the storage of a `BytesMut`, or a `Vec<u8>`.
Splitting or advancing a `Vec` page copies its data, so prefer the other
kinds when output will be split or shared. Queuing an owned `Vec` directly
can still avoid an initial copy; that is different from converting the
vector to `Bytes` first.

Keep data paged for as long as the consumer accepts chunks. Calling
`freeze()` before passing several pages to a chunk-aware transport combines
them into one allocation and loses the benefit of paging.

Paging does not limit queued output. A producer can outpace the transport
and retain many pages, including large buffers appended without copying.
Stop producing when the I/O layer reports write backpressure.

`BytePages::default()` uses `Size16`. The page size cannot be `Unset`;
[`BytePages::new`] panics on it, as does [`BytePages::set_page_size`].
Changing the size affects future page allocations, leaving the current page
and queued data alone.

Creating an empty queue does not immediately allocate a payload page.
Page buffers are allocated lazily. The page list itself is reused too;
each thread keeps up to 128 empty lists for new `BytePages` values.

## Buffers in the I/O layer

The [I/O Abstraction Layer](./7-io.md) uses these buffers for connection
input and output. You can tune their page sizes through [`IoConfig`].
Reads and writes share the same per-thread page cache.

The transport reads into a pooled `BytesMut`. Codecs use `split_to` to
hand off frames. Large decoded messages share the read buffer; tiny ones
are copied inline.

The connection adapts the requested size of new read buffers to the read
load. This target stays between the minimum and maximum read sizes in
`IoConfig`, `Size4` and `Size64` by default. The page cache can supply a
page one class smaller, as described in [Pooled buffers](#pooled-buffers).
A buffer grows when less than
`BytePageSize::low()` of its page remains free. Larger input moves it to
bigger classes; beyond `Size256`, it becomes unpooled and is freed when
empty.

Encoders write into `BytePages`. [`IoConfig::write_size`] sets its page
size, `Size16` by default. Use `IoRef::encode_bytes` to append owned buffers
without copying large payloads. The write threshold controls when a transport
may start writing while output is still being produced. By default, it is
`half_capacity()` of the page size.

An empty read buffer is released immediately. Its page returns to the cache
only when the last frame split from it is dropped. Holding a whole request
or frame keeps the full page alive. Copy the field when only a small part
needs to survive.

[`set_page_cache_size`] tunes the cache for both read and write pages.

### What memory usage means in practice

High memory usage can mean several different things. Start with live
data: bytes still owned by requests, responses, transports or application
state. Pending work needs this data, so reducing the work in flight can
reduce its memory use.

Retained allocations are a different cost. A small live view may keep a
much larger shared buffer alive. If that view is intentionally long-lived,
`trimdown` can reduce the retained storage without discarding its data.

Cached allocations hold empty pages or read buffers for reuse. Cache
limits control this category, not live data. Both retention and caching
can keep resident memory high after a traffic spike without indicating an
ownership leak. The tracker below distinguishes memory still owned by
buffers from pages that have returned to the cache.

## Tracking buffer memory

Use the `tracker` feature to see which part of your application keeps buffer
memory alive. You can charge any heap allocation to a [`BytesTracker`].
A tracker can represent a cache, a tenant or another subsystem, down to a
single connection.

The charge follows the allocation until its last handle is dropped. A single
frame in a cache can keep a large buffer alive long after the allocating
code has moved on. The allocator reports that memory as in use; the tracker
helps you identify who owns it.

The feature is off by default. Enable it on `ntex` to turn it on in the I/O
and network layers too. This also re-exports `BytesTracker` and
[`BytesTrackerSnapshot`] in `ntex::util`:

```toml
ntex = { version = "4", features = ["tracker"] }
```

If you use `ntex-bytes` on its own, enable `ntex-bytes/tracker` instead.
The I/O layer can charge connection read buffers to trackers; see the
[read-buffer example](#example-read-buffers-in-the-io-layer).

The feature has a small cost even for buffers that are never charged. Every
heap buffer's header grows from 16 to 32 bytes to hold a tracker pointer.
On 32-bit targets it grows to 20 bytes.

The tracker updates its atomic counters when a charged allocation is
created, grown or released, not when another handle simply shares it.
Freezing a tiny buffer can release its allocation because the result is
stored inline.

### How charges work

Call `snapshot()` to read the memory charged to a tracker. All handles of
the tracker share the same atomic counters. The snapshot contains two
numbers:

- `live_bytes`: the total size of the charged allocations, including headers.
  It measures allocation size rather than data length. A `Size4` buffer
  counts as 4096 bytes whether empty or full.
- `live_allocations`: how many allocations are charged.

Charges follow the [allocation, not the handle](#the-mental-model).
A frame split off a buffer keeps the whole allocation charged for as long
as it keeps that allocation alive. If the mutable buffer grows into a new
allocation, the new one uses the same tracker. Shared frames may keep the
old allocation charged too.

The charge ends when the last handle is dropped, on whichever thread drops
it. A page returns to the cache uncharged; its next user can charge it
again. Copies do not inherit a charge. That includes a cloned `BytesMut`,
small data stored inline in `Bytes`, copies made by `trimdown`, and data
converted into other types.

```rust
use ntex_bytes::{BytePageSize, BytesMut, BytesTracker};

let tracker = BytesTracker::new();

let mut buf = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
buf.extend_from_slice(&[b'x'; 1000]);
let frame = buf.split_to(600); // shares the allocation
let tag = frame.slice(..8); // short enough to be copied inline
drop(buf);

// `frame` alone keeps the whole page charged
let snap = tracker.snapshot();
assert_eq!(snap.live_allocations, 1);
assert_eq!(snap.live_bytes, 4096);

// `tag` is a copy, so dropping `frame` ends the charge
drop(frame);
assert_eq!(tracker.snapshot().live_bytes, 0);
assert_eq!(&tag[..], b"xxxxxxxx");
```

Compared with the categories in
[What memory usage means in practice](#what-memory-usage-means-in-practice),
a tracker counts live data and retained allocations. It never counts cached
ones. If tracked memory drops after a traffic spike but resident memory
stays high, check page-cache metrics before treating that difference as a
leak. A tracker does not account for every allocation in the process.

### Building your own tracker

Create a tracker for the part of your application whose memory you want to
measure. For example, track a response cache, a per-tenant queue or a
decompression stage. For a new buffer, pass the tracker to
[`BytesMut::with_page_size_in`]. It uses the page cache like
`with_page_size`, including treating `BytePageSize::Unset` as a request for
`Size64`.

For a buffer you already have, call [`BytesMut::set_tracker`]. If another
tracker owns its charge, the charge moves, even while `Bytes` frames split
from the allocation are still alive. Setting the same tracker again does
nothing.

[`BytesMut::tracker`] tells you which tracker a buffer is charged to.
Only a `BytesMut` can change the charge. Set it before freezing the buffer
and handing out `Bytes`.

This cache charges the buffers it stores to its own tracker:

```rust
use std::collections::HashMap;
use ntex_bytes::{Bytes, BytesMut, BytesTracker, BytesTrackerSnapshot};

#[derive(Default)]
struct BodyCache {
    items: HashMap<String, Bytes>,
    tracker: BytesTracker,
}

impl BodyCache {
    fn insert(&mut self, key: String, mut body: BytesMut) {
        // moves the charge from any previous tracker to the cache
        body.set_tracker(&self.tracker);
        self.items.insert(key, body.freeze());
    }

    fn usage(&self) -> BytesTrackerSnapshot {
        self.tracker.snapshot()
    }
}
```

The cache keeps whole allocations alive, so its charge includes spare
capacity. Entries that hold small slices of large buffers will show up in
the numbers. Use `trimdown` before storing them to reduce that retention.

You can read the cache's usage from a metrics task on another thread.
`BytesTracker` is `Send` and `Sync`, and its clones share the same counters.
Use `ptr_eq` to check whether two handles refer to the same tracker. The
tracker lives while any handle or charged allocation exists, so dropping
the cache's handle does not affect buffers still alive elsewhere.

Trackers report usage; they do not enforce limits. To enforce a budget,
check `snapshot()` before accepting more data. Its two counters are read
one after the other, so a snapshot taken during allocation or release is
only approximately consistent.

### Example: read buffers in the I/O layer

To track connection input without charging each buffer yourself, choose a
policy in [`IoConfig`]. Set it with [`IoConfig::set_buf_tracker`]. `ntex-io`
uses the same tracker API to charge every read buffer a connection allocates.

Choose [`BufTracker::Shared`][`BufTracker`] to measure a listener or
service across all workers. Choose `PerConnection` to give each connection
its own tracker, created on first use. The default, `Disabled`, leaves
read buffers uncharged. Here, all connections using `cfg` charge the same
tracker:

```rust
use ntex::{SharedCfg, io::{BufTracker, IoConfig}, util::BytesTracker};

let api_input = BytesTracker::new();
let cfg = SharedCfg::new("API").add(
    IoConfig::new().set_buf_tracker(BufTracker::Shared(api_input.clone())),
);
```

Keep the clone and read `api_input.snapshot()` from a metrics task. Frames
split off a read buffer keep it charged. The number includes every request
body or message the application still holds, even after its connection has
closed. The I/O metrics no longer see that input.

Only read buffers are charged; write buffers are not. With `PerConnection`,
[`IoRef::buf_tracker`] returns the connection's tracker.
`IoContext::buf_tracker` does the same inside filters. You can use the
tracker to close a client that holds too much input.

Do not use the reference after `Io::set_config`. Clone the tracker if you
need it for longer. The clone keeps reporting buffers that outlive the
connection without keeping the connection alive.

You can also charge your own buffers to a connection's tracker. For example,
you might assemble a body from many frames or decompress a payload. Pass
the tracker from `io.buf_tracker()` to `with_page_size_in`. That memory
appears alongside the connection's read buffers because the tracker is an
ordinary `BytesTracker`.

A `Shared` tracker uses a single cache line updated by every worker thread.
It becomes contended under load, so prefer one tracker per listener or
service over one for the whole server.

The [server chapter](./5-server.md#io-buffer-attribution) explains how TLS
and io-uring buffers are counted and how `Io::set_config` moves charges.
It also shows how to read tracker numbers alongside I/O metrics.

## Performance guidelines

Choose buffer operations based on lifetime as well as size. A decoded
frame going straight to a consumer is a good fit for `split_to` or `take`,
so large payloads can share the input allocation. A field entering a
long-lived cache is a better candidate for `trimdown` or an explicit copy.
Drop shared frames as soon as you no longer need them: a cheap clone can
extend the lifetime of a large allocation. Inline clones instead copy a
few bytes without allocation or reference counting.

For output, reserve once when you know the message size, or start with
`BytesMut::with_capacity`. Use `with_page_size` with a class matching the
usual size for short-lived I/O buffers; cache hits avoid fresh payload
allocations. Keep an existing large body separate with `BytePages::append`
rather than copying it with `put_slice`. An owned `Vec<u8>` can go directly
into that queue too, avoiding the copy of converting it to `Bytes` first.
For constant protocol bytes, `from_static` neither allocates nor copies.

Measure allocation volume and retained memory alongside throughput. Fewer
copies do not necessarily mean less memory retained between requests.

[`Buf`]: https://docs.ntex.rs/api/ntex_bytes/buf/trait.Buf.html
[`BufMut`]: https://docs.ntex.rs/api/ntex_bytes/buf/trait.BufMut.html
[`BufTracker`]: https://docs.ntex.rs/api/ntex/io/enum.BufTracker.html
[`BytePage`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePage.html
[`BytePageSize`]: https://docs.ntex.rs/api/ntex_bytes/enum.BytePageSize.html
[`BytePageSize::for_capacity`]: https://docs.ntex.rs/api/ntex_bytes/enum.BytePageSize.html#method.for_capacity
[`BytePageSize::next`]: https://docs.ntex.rs/api/ntex_bytes/enum.BytePageSize.html#method.next
[`BytePageSize::prev`]: https://docs.ntex.rs/api/ntex_bytes/enum.BytePageSize.html#method.prev
[`BytePages`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html
[`BytePages::append`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.append
[`BytePages::copy_to`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.copy_to
[`BytePages::freeze`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.freeze
[`BytePages::move_to`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.move_to
[`BytePages::new`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.new
[`BytePages::prepend`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.prepend
[`BytePages::set_page_size`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.set_page_size
[`BytePages::split_into`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.split_into
[`BytePages::split_to`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.split_to
[`BytePages::take`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytePages.html#method.take
[`ByteString`]: https://docs.ntex.rs/api/ntex_bytes/struct.ByteString.html
[`ByteString::from_bytes_unchecked`]: https://docs.ntex.rs/api/ntex_bytes/struct.ByteString.html#method.from_bytes_unchecked
[`ByteString::into_bytes`]: https://docs.ntex.rs/api/ntex_bytes/struct.ByteString.html#method.into_bytes
[`ByteString::trimdown`]: https://docs.ntex.rs/api/ntex_bytes/struct.ByteString.html#method.trimdown
[`Bytes`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html
[`Bytes::from_ext`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html#method.from_ext
[`Bytes::from_static`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html#method.from_static
[`Bytes::is_inline`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html#method.is_inline
[`Bytes::slice`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html#method.slice
[`Bytes::slice_checked`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html#method.slice_checked
[`Bytes::split_off_checked`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html#method.split_off_checked
[`Bytes::split_to`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html#method.split_to
[`Bytes::split_to_checked`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html#method.split_to_checked
[`Bytes::trimdown`]: https://docs.ntex.rs/api/ntex_bytes/struct.Bytes.html#method.trimdown
[`BytesMut`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html
[`BytesMut::advance_to`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.advance_to
[`BytesMut::capacity`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.capacity
[`BytesMut::clear`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.clear
[`BytesMut::freeze`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.freeze
[`BytesMut::is_unique`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.is_unique
[`BytesMut::new`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.new
[`BytesMut::page_size`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.page_size
[`BytesMut::reserve`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.reserve
[`BytesMut::reserve_exact`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.reserve_exact
[`BytesMut::reserve_more`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.reserve_more
[`BytesMut::set_tracker`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.set_tracker
[`BytesMut::split_to`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.split_to
[`BytesMut::split_to_checked`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.split_to_checked
[`BytesMut::take`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.take
[`BytesMut::tracker`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.tracker
[`BytesMut::with_capacity`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.with_capacity
[`BytesMut::with_page_size`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.with_page_size
[`BytesMut::with_page_size_in`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesMut.html#method.with_page_size_in
[`BytesTracker`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesTracker.html
[`BytesTrackerSnapshot`]: https://docs.ntex.rs/api/ntex_bytes/struct.BytesTrackerSnapshot.html
[`IoConfig`]: https://docs.ntex.rs/api/ntex/io/struct.IoConfig.html
[`IoConfig::set_buf_tracker`]: https://docs.ntex.rs/api/ntex/io/struct.IoConfig.html#method.set_buf_tracker
[`IoConfig::write_size`]: https://docs.ntex.rs/api/ntex/io/struct.IoConfig.html#method.write_size
[`IoRef::buf_tracker`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.buf_tracker
[`ntex::util`]: https://docs.ntex.rs/api/ntex/util/index.html
[`set_page_cache_size`]: https://docs.ntex.rs/api/ntex_bytes/fn.set_page_cache_size.html
[`StorageExt`]: https://docs.ntex.rs/api/ntex_bytes/trait.StorageExt.html
