# I/O Abstraction Layer

You can write protocol and service code without tying it to a runtime or
reactor such as Tokio, Compio, or Neon. The ntex I/O layer gives that code a
common interface to the connection. It also handles buffered reads and
writes, backpressure, timeouts, and graceful shutdown.

## Socket abstractions

Protocol code works with [`Io`] rather than a concrete socket type. A runtime
adapter moves bytes between its socket and buffers owned by `Io`. The I/O
subsystem does not call Tokio, Compio, or Neon socket APIs directly.

Each direction has an in-memory buffer owned by `Io`. The read half carries
bytes from the transport to the application, and the write half carries them
back. Owning the buffers lets `Io` apply backpressure and run filters as
bytes pass through.

You normally pass an `Io` to a protocol service such as
[`ntex::http::HttpService`] or `ntex_mqtt::Server`. The service decodes input
into protocol messages and encodes responses into the write buffer. It does
not depend on the concrete socket type. The [`ntex-net`] crate provides the
runtime-specific implementations for Tokio, Compio, and Neon.

You only need to work below this interface when integrating a transport. If
you use an existing backend, you can move on to
[configuration and timeouts](#configuration-and-timeouts).

To integrate a socket type, implement [`IoStream`]. Its `start()` method
receives an [`IoContext`] and starts the transport-specific tasks. It returns
a [`Handle`] for controlling or querying the transport.

The work on an active connection falls into these roles:

1. A protocol or dispatcher task consumes decoded input and queues encoded
   output through `Io` or [`IoRef`]. It typically runs the protocol service
   and, indirectly, application code.
2. A transport read task waits for read readiness, reads bytes from the
   socket, and submits them to the I/O subsystem.
3. A transport write task takes queued bytes from the I/O subsystem and
   writes them to the socket.

The runtime adapter decides how to schedule this work. Built-in adapters
usually use cooperating read and write tasks. That scheduling is an
implementation detail, not a requirement of `IoStream`.

The read task first waits for [`IoContext::poll_read_ready`] to permit more
input. This keeps socket reads in step with ntex backpressure.

Use [`IoContext::take_read_buf`] to obtain the buffer for a read. After the
read, pass the buffer and result to [`IoContext::release_read_buf`]. ntex then
processes filters, wakes the dispatcher, and pauses further reads when the
configured high-water mark is reached.

For synchronous reads that do not hold the buffer across an await, use
[`IoContext::with_read_buf`] instead. It combines taking and releasing the
buffer into one call. It borrows the buffer in place, without detaching it and
appending it back.

The write task waits for [`IoContext::poll_write_ready`] before working on
output. Use [`IoContext::with_write_dst`] to access the queued data. Then
report how many bytes reached the peer through
[`IoContext::update_write_status`]. ntex uses that report to manage write
backpressure, resume waiting services, and coordinate graceful shutdown.

A transport can also report bytes it has taken before it writes them.
Completion-based backends take ownership of whole pages until an operation
completes. Those bytes have left the write buffer, but they still count as
outstanding output. They remain outstanding until the transport reports them
as written or returns them to the buffer. Flush completion, write
backpressure, and the shutdown drain all include those bytes.

### Readiness and transport teardown

An adapter must distinguish queued work from a socket that is ready to do it.
The buffer-release and write-status methods return an [`IoTaskStatus`]. Its
`Io` status means work remains, not that the transport is ready. Re-arm
transport readiness before the next operation.

On writes, the status is `Io` whenever output remains buffered, even if the
last attempt made no progress. Output already owned by the transport does not
keep the task running. Its completion wakes the task again.

The two `IoContext` readiness methods instead return a [`Readiness`] value.
`Readiness::Ready` lets the task perform its next operation; the other values
tell it how to tear down the connection.

`Readiness::Close` tells the task to close both directions and release the
socket, cancelling any operations still in flight. It covers graceful
shutdown, I/O or filter failure, and an expired shutdown deadline. On the
graceful path, ntex reports it only after draining buffered output, so nothing
remains to flush.

For a socket, closing means `shutdown(SHUT_RDWR)` followed by `close()`.
Discard any data left in the receive queue first. Closing with unread input
aborts the connection with an RST and loses the output that was just drained.

`Readiness::Terminate` occurs only after an explicit force close through
[`IoRef::terminate`]. It deliberately discards buffered data. The task must
abort the connection rather than close gracefully, without draining the
receive queue or calling `shutdown(SHUT_RDWR)`.

For a socket, set `SO_LINGER` to zero before `close()`. The peer then sees an
RST and cannot mistake a truncated stream for a complete one.

If a transport fails or decides to abort, call [`IoContext::stop`] with the
error. This terminates the connection without draining pending work.

Once transport teardown finishes, call [`IoContext::stopped`], regardless of
which side initiated it. That report completes [`Io::shutdown`]. Finishing
filter shutdown and flushing alone does not complete it; the backend must
report that the connection has stopped.

The following example sketches a Tokio-style adapter. Real adapters use their
runtime's native readiness and buffer APIs. Only the adapter accesses the
socket; application and protocol code use `Io` and `IoRef`. Buffer limits and
read/write backpressure apply consistently across supported runtimes.

```rust,ignore
impl IoStream for TcpStream {
    fn start(self, ctx: IoContext) -> Box<dyn Handle> {
        let socket = Rc::new(self);

        tokio::task::spawn_local(read_task(socket.clone(), ctx.clone()));
        tokio::task::spawn_local(write_task(socket.clone(), ctx));

        Box::new(SocketHandle(socket))
    }
}

async fn read_task(socket: Rc<TcpStream>, ctx: IoContext) {
    loop {
        // resolves `IoContext::poll_read_ready`
        match wait_for_read_readiness(&ctx).await {
            Readiness::Ready => {}
            Readiness::Close | Readiness::Terminate => break,
        }

        let mut buf = ctx.take_read_buf();
        let result = read_from_socket(&socket, &mut buf);

        match ctx.release_read_buf(buf, result) {
            IoTaskStatus::Io => {}
            IoTaskStatus::Pause => wait_for_read_resume(&ctx).await,
            IoTaskStatus::Stop => break,
        }
    }
}

async fn write_task(socket: Rc<TcpStream>, ctx: IoContext) {
    let terminate = loop {
        // resolves `IoContext::poll_write_ready`
        match wait_for_write_readiness(&ctx).await {
            Readiness::Ready => {}
            Readiness::Close => break false,
            Readiness::Terminate => break true,
        }

        let result = ctx.with_write_dst(|buf| {
            write_to_socket(&socket, buf)
        });

        // `result` carries the number of bytes that reached the peer
        match ctx.update_write_status(result) {
            IoTaskStatus::Io => {}
            IoTaskStatus::Pause => wait_for_queued_output(&ctx).await,
            IoTaskStatus::Stop => break false,
        }
    };

    if terminate {
        // force close, abort so that the peer sees an RST
        abort_socket(&socket);
    } else {
        // close both directions
        shutdown_socket(&socket).await;
    }
    // report teardown as complete
    ctx.stopped(None);
}
```

[`Handle`]: https://docs.ntex.rs/api/ntex/io/trait.Handle.html
[`Io`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html
[`IoContext`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html
[`IoContext::poll_read_ready`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html#method.poll_read_ready
[`IoContext::poll_write_ready`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html#method.poll_write_ready
[`IoContext::stop`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html#method.stop
[`IoContext::stopped`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html#method.stopped
[`IoContext::with_read_buf`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html#method.with_read_buf
[`IoContext::take_read_buf`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html#method.take_read_buf
[`IoContext::release_read_buf`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html#method.release_read_buf
[`IoContext::update_write_status`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html#method.update_write_status
[`IoContext::with_write_dst`]: https://docs.ntex.rs/api/ntex/io/struct.IoContext.html#method.with_write_dst
[`IoRef`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html
[`IoStream`]: https://docs.ntex.rs/api/ntex/io/trait.IoStream.html
[`IoTaskStatus`]: https://docs.ntex.rs/api/ntex/io/enum.IoTaskStatus.html
[`Readiness`]: https://docs.ntex.rs/api/ntex/io/enum.Readiness.html
[`ntex::http::HttpService`]: https://docs.ntex.rs/api/ntex/http/struct.HttpService.html

[`ntex-net`]: https://docs.ntex.rs/api/ntex_net/index.html

### Transport handles and metadata

You may need transport information even though `Io` hides the socket. The
[`Handle`] returned by `IoStream::start()` exposes metadata through typed
queries. It can also resume transport writes on request.

Each backend chooses which query types to support. The built-in network
backends expose the remote socket address as [`PeerAddr`]. Use
[`IoRef::query`] to request it; you can also call this method on `Io` through
dereferencing. The returned [`QueryItem`] provides the value through `get()`:

```rust
use ntex::io::{Io, types::PeerAddr};

fn log_peer_addr(io: &Io) {
    if let Some(addr) = io.query::<PeerAddr>().get() {
        println!("Peer address {:?}", addr.into_inner());
    }
}
```

A query passes through the filter stack before it reaches the transport
handle. Filters can expose their own typed metadata along the way.

[`IoRef::query`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.query
[`PeerAddr`]: https://docs.ntex.rs/api/ntex/io/types/struct.PeerAddr.html
[`QueryItem`]: https://docs.ntex.rs/api/ntex/io/types/struct.QueryItem.html

## Configuration and timeouts

Start with the default buffering and timeouts, then use [`IoConfig`] when a
workload needs different limits. You normally add it to a [`SharedCfg`]. When
you create a connection, `Io::new()` reads the `IoConfig` from that shared
configuration, or uses defaults if it is absent.

This example combines the settings in one place; you rarely need to change
all of them:

```rust
use ntex::{
    SharedCfg,
    io::IoConfig,
    time::{Millis, Seconds},
    util::BytePageSize,
};

let cfg = SharedCfg::new("my-protocol")
    .add(
        IoConfig::new()
            .set_connect_timeout(Millis(5_000))
            .set_keepalive_timeout(Seconds(30))
            .set_shutdown_timeout(Seconds(2))
            .set_frame_read_rate(Seconds(2), Seconds(10), 1_024)
            .set_write_timeout(Seconds(10))
            .set_read_size(BytePageSize::Size4, BytePageSize::Size32)
            .set_read_backpressure(32 * 1024)
            .set_write_backpressure(32 * 1024)
            .set_write_buf_threshold(8 * 1024),
    )
    .build();
```

### Timeouts

Timeouts protect different stages of a connection. The `ntex-net` crate uses
the connection timeout while resolving and opening an outgoing connection.
Once connected, protocol dispatchers handle the keep-alive timeout and frame
read-rate limits. A read-rate limit protects a decoder from peers that send
an incomplete frame too slowly. It also covers the first frame, so a peer
that connects and stays silent is closed when the limit expires.

A peer can also stop reading your output. The write timeout bounds how long
write backpressure can remain enabled before the dispatcher stops with a
write timeout. Its timer runs from the moment backpressure is enabled until
it is disabled; keep-alive and read-rate timers do not run during that time.
After backpressure is disabled, only keep-alive bounds any remaining output
in the dispatcher. Set a write timeout especially for untrusted peers:
without one, a peer that stops reading can keep a backpressured connection
and its output alive indefinitely.

When you use `Io` directly, the write timeout also bounds each wait in
`send()`, `flush()`, and `write_ready()`. If it expires, the wait fails and
leaves the connection open for you to close.

The HTTP/1 dispatcher has its own keep-alive, read-rate, and write timeout
settings. Configure those through [`HttpServiceConfig`], including its own
`set_write_timeout()`, rather than the corresponding `IoConfig` settings.

Finally, the graceful-shutdown timeout covers filter shutdown and the
transport drain of pending output together. You cannot disable it: a zero
value is rejected. It defaults to one second. Connection, keep-alive, and
write timeouts and frame read-rate limits are all disabled by default.

### Buffer limits

Buffering has two separate controls: page sizes govern allocation, while
high-water marks tell producers or transports when to pause. A watermark is
not a hard memory limit, because one socket read or encoded item can cross
it. The default read and write high-water marks are about 32 KiB and 16 KiB,
respectively.

Write backpressure ends when outstanding output falls to half its high-water
mark. This count includes both buffered output and bytes owned by the
transport but not yet written to the peer.

The minimum and maximum read page sizes bound the adaptive size of new read
buffers. They control allocation and read-call frequency, not how much unread
data a connection can queue. Increase the maximum for sustained large frames
or bodies; keep the minimum small for many mostly idle connections. Before
another socket read, a buffer grows when less than [`BytePageSize::low`] of
its page remains free.

On the write side, the page size controls newly allocated [`BytePages`], which
hold output; write backpressure takes only a high-water mark. The write
threshold tells supported transports when to attempt an early direct write.
It is a latency hint for starting transport work earlier, not a flush or a
replacement for write backpressure.

### Reconfiguring a connection

A protocol upgrade may change a connection's timeout or buffer requirements.
Use [`Io::set_config`] to switch an established connection to another shared
configuration.

The method is `unsafe`. Replacing the configuration may release the
allocation behind references returned by [`IoRef::cfg`]. No such reference
may remain live across the call or be used afterwards.

[`Io::set_config`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.set_config
[`IoRef::cfg`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.cfg
[`SharedCfg`]: https://docs.ntex.rs/api/ntex/struct.SharedCfg.html

## Filter subsystem

A protocol service may need a transformed byte stream rather than raw socket
bytes. TLS decrypts incoming records and encrypts outgoing data. A protocol
can also run inside another framing layer, as MQTT does over WebSocket.

Use [`FilterLayer`] for these transformations. Filters work on in-memory
buffers and do not perform socket I/O. You can use the same filter with any
supported runtime backend.

[`FilterLayer::process_read_buf`] transforms bytes received from the next
inner layer. [`FilterLayer::process_write_buf`] transforms application output
before passing it toward the transport.

You can compose filters with [`Io::add_filter`]. It adds a layer and allocates
the intermediate read and write buffers between adjacent layers. For
example, an MQTT service can use either of these stacks:

```text
socket <-> TLS <-> MQTT

socket <-> TLS <-> WebSocket <-> MQTT
```

Read bytes move from the socket through the inner filters toward the
application. Write bytes move in the opposite direction. In the second stack,
the WebSocket filter removes and creates WebSocket framing. The TLS filter
decrypts and encrypts the resulting stream.

The MQTT service still reads and writes MQTT bytes. It does not need to know
which filters sit below it. The [TLS chapter](10-tls.md) shows this model in
HTTP clients, WebSocket clients, and servers. It also covers backend
configuration and peer certificate authentication.

Filters can keep protocol state and expose typed metadata through `query()`.
A TLS filter, for example, can expose the negotiated protocol or peer
certificate. Filters also participate in graceful shutdown; a WebSocket
filter can generate a close frame then.

A filter can emit output while processing input. ntex pushes that output
toward the transport as soon as read processing finishes, without waiting for
the application to write.

[`FilterLayer`]: https://docs.ntex.rs/api/ntex/io/trait.FilterLayer.html
[`FilterLayer::process_read_buf`]: https://docs.ntex.rs/api/ntex/io/trait.FilterLayer.html#tymethod.process_read_buf
[`FilterLayer::process_write_buf`]: https://docs.ntex.rs/api/ntex/io/trait.FilterLayer.html#tymethod.process_write_buf
[`Io::add_filter`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.add_filter

Most byte transformations need only `FilterLayer`. To observe or control the
whole chain, wrap it with a lower-level [`Filter`] through
[`Io::map_filter`].

`Filter` handles buffer processing, queries, and shutdown. It also takes part
in read and write readiness decisions. A wrapper can delay readiness to
throttle traffic, then wake the I/O tasks when work can resume. It can also
observe buffer processing for metrics or accounting without changing bytes.

A custom `Filter` normally stores the filter it wraps. Delegate each operation
you do not intend to override. ntex provides forwarding macros for readiness,
queries, and shutdown to make this less error-prone.

[`Filter`]: https://docs.ntex.rs/api/ntex/io/trait.Filter.html
[`Io::map_filter`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.map_filter

### Typed versus erased filter stacks

Keep a concrete filter stack type while building a connection. At a service
boundary, you may need one I/O type for connections with different stacks.
Type erasure gives you that common type without changing connection behavior.

[`Io`] stores the stack in its type parameter. A new connection starts as
`Io<Base>`, with the [`Base`] filter. Calling `add_filter(layer)` consumes the
value and returns `Io<Layer<U, F>>`. The [`Layer`] marker pairs the new outer
layer `U` with the previous stack `F`.

The concrete type provides static dispatch. [`Io::filter`] can also return
the concrete outer filter:

```rust,ignore
let io: Io<Base> = create_io();
let io: Io<Layer<MyFilter, Base>> = io.add_filter(MyFilter::new());
```

Use [`Io::seal`] to erase the stack type and get `Io<Sealed>`, using the
[`Sealed`] marker. [`Io::boxed`] returns the [`IoBoxed`] convenience wrapper.
Both consume the original `Io` and keep the same connection state and filter
behavior behind a dynamically dispatched `Filter`.

```rust,ignore
let io: IoBoxed = io.boxed();
start_protocol(io);
```

You can add typed layers to a sealed stream and erase the result again.
Normally, erase the type where a protocol or service needs uniform I/O,
rather than while you are building the stack.

[`Base`]: https://docs.ntex.rs/api/ntex/io/struct.Base.html
[`Io::boxed`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.boxed
[`Io::filter`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.filter
[`Io::seal`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.seal
[`IoBoxed`]: https://docs.ntex.rs/api/ntex/io/struct.IoBoxed.html
[`Layer`]: https://docs.ntex.rs/api/ntex/io/struct.Layer.html
[`Sealed`]: https://docs.ntex.rs/api/ntex/io/struct.Sealed.html

## Read/write streams

Choose read and write APIs based on how your protocol handles messages.
Incoming and outgoing bytes follow separate buffer paths. Queuing bytes and
waiting for transport progress are separate operations.

### Reading

A codec or protocol service reads from the application-facing buffer. The
transport adapter fills a [`BytesMut`] with socket input, then the bytes pass
through the filter chain into that buffer.

Choose the highest-level read API that fits your protocol:

- Use `recv(codec)` for framed messages. It decodes buffered input and waits
  when the codec reports an incomplete frame.
- Use `read_exact()` for a fixed-size header or field.
- For a custom parser, inspect or consume the buffer through `IoRef`. Call
  [`Io::read_more`] only when you have decided that you need more bytes.
  It actively resumes transport reads and releases read backpressure, even
  if the buffer is still large. Use [`IoRef::read_dst_size`] for a passive
  size check.

Read backpressure depends on the size of the application-facing buffer.
When it reaches the configured high-water mark, ntex pauses the transport
read task. Consuming input through these methods wakes the task once the
buffer falls to half that mark:

- [`IoRef::decode`]
- [`IoRef::with_buf`]
- [`IoRef::with_read_src`]
- [`IoRef::with_read_dst`]

[`Io::recv`] and [`Io::read_exact`] wait for more input. They release
backpressure regardless of how much input remains buffered.

`BytesMut` is contiguous and growable. A decoder can split immutable [`Bytes`]
values from it without copying larger payloads. Short values are copied into
the `Bytes` handle itself. A decoded message can therefore retain part of the
input while the decoder continues with later data.

The remaining read-buffer details matter when tuning allocation. Read
buffers use `ntex-bytes` pages, with each connection choosing new buffers'
page size between the minimum and maximum set by [`IoConfig::set_read_size`].
The defaults are `Size4` and `Size64`. A connection starts at the minimum, so
idle and light connections pin small pages.

Consecutive reads that fill their buffer form a batch, together with the read
that ends the sequence. If a batch is larger than the page, the page size
grows to fit it, up to the maximum.

Once traffic becomes lighter, the page size shrinks by one step after four
consecutive batches fit in half of the next smaller page. It never shrinks
below the minimum. [`Io::set_config`] restarts it at the minimum. If you want
a fixed page size instead, set equal minimum and maximum sizes.

Page size does not affect read backpressure. The default read high-water mark
is the `Size32` capacity, 32,752 bytes. Use
[`IoConfig::set_read_backpressure`] to change it.

An empty buffer returns to the per-thread page cache for its size. Write
pages and other pooled `BytesMut` buffers share that cache; configure it with
[`set_page_cache_size`]. A frame split from the read buffer keeps the page
alive. The page returns to the cache when the last frame is dropped.

Before another socket read, the adapter obtains a buffer from `IoContext`.
ntex calls [`BytesMut::reserve_more`] when less than [`BytePageSize::low`] of
the page remains free. It compacts data within the page if the data still fits.

If larger input must be buffered, the buffer moves to bigger page sizes.
Beyond the largest page size, it uses a plain allocation. That allocation is
freed rather than cached.

### Writing

Queue output first, then decide how long to wait for it. Encoding methods make
bytes available to the transport write task; they do not wait for every byte
to reach the socket.

Use [`IoRef::encode`] to queue codec-encoded output. [`IoRef::encode_slice`]
and [`IoRef::encode_bytes`] also queue output. Queueing schedules the
transport write task when it needs to resume.

On transports that support direct writes, the write threshold can trigger a
write while your application is still producing output. This can reduce
latency for large responses.

Use [`Io::flush`] to choose the write wait policy:

- `flush(false)` waits for backpressure to ease, rather than draining all
  output. It returns immediately below the high-water mark. Once that mark
  is reached, it waits until outstanding output falls to half the mark.
- `flush(true)` waits until all queued data reaches the peer. This includes
  bytes owned by the transport but not yet written.

[`Io::send`] combines codec encoding with a full flush.

Write backpressure is advisory. The `encode` methods keep accepting output
after the high-water mark is reached. Refusing half of a protocol message
would be difficult to recover from.

If a dispatcher does not manage your producer, await [`IoRef::write_ready`]
before queueing each next chunk. You can also use `flush(false)` between
batches:

```rust
use ntex::{
    io::Io,
    util::Bytes,
};

async fn queue_chunk(io: &Io, chunk: Bytes) -> std::io::Result<()> {
    io.write_ready().await?;
    io.encode_bytes(chunk)
}
```

This keeps normal streaming output around the configured watermark. It does
not split a large chunk or make the watermark a strict per-connection memory
cap.

These APIs all queue output in [`BytePages`], a growable collection of byte
pages. Newly allocated pages use the size configured by [`IoConfig`]. Owned
buffers passed to `IoRef::encode_bytes` can become pages directly when their
storage can be retained, avoiding a copy.

Write filters consume the pages in order. They transform the contents and
place the result in the next buffer toward the transport.

[`BytePages`]: https://docs.ntex.rs/api/ntex/util/struct.BytePages.html
[`BytePageSize::low`]: https://docs.ntex.rs/api/ntex/util/enum.BytePageSize.html#method.low
[`Bytes`]: https://docs.ntex.rs/api/ntex/util/struct.Bytes.html
[`BytesMut`]: https://docs.ntex.rs/api/ntex/util/struct.BytesMut.html
[`BytesMut::reserve_more`]: https://docs.ntex.rs/api/ntex/util/struct.BytesMut.html#method.reserve_more
[`Io::flush`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.flush
[`Io::read_exact`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.read_exact
[`Io::read_more`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.read_more
[`Io::recv`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.recv
[`Io::send`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.send
[`IoConfig`]: https://docs.ntex.rs/api/ntex/io/struct.IoConfig.html
[`IoConfig::set_read_backpressure`]: https://docs.ntex.rs/api/ntex/io/struct.IoConfig.html#method.set_read_backpressure
[`IoConfig::set_read_size`]: https://docs.ntex.rs/api/ntex/io/struct.IoConfig.html#method.set_read_size
[`HttpServiceConfig`]: https://docs.ntex.rs/api/ntex/http/struct.HttpServiceConfig.html
[`IoRef::decode`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.decode
[`IoRef::encode`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.encode
[`IoRef::encode_bytes`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.encode_bytes
[`IoRef::encode_slice`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.encode_slice
[`IoRef::read_dst_size`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.read_dst_size
[`IoRef::write_ready`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.write_ready
[`IoRef::with_read_dst`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.with_read_dst
[`IoRef::with_read_src`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.with_read_src
[`IoRef::with_buf`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.with_buf
[`set_page_cache_size`]: https://docs.ntex.rs/api/ntex_bytes/fn.set_page_cache_size.html

## Connection lifecycle and shutdown

A protocol service needs to notice connection changes even while it waits on
a dependency or a slow response. It also needs to give pending output a
chance to reach the peer when closing. A connection stays usable until the
application closes it, the peer disconnects, or the transport reports an
error.

Use [`Io::poll_status_update`] while waiting on work other than input. It
reports an [`IoStatusUpdate`] value:

- `Timeout` means the dispatcher timer expired, such as the keep-alive timer.
- `WriteBackpressure` means outstanding output reached the write high-water
  mark. Stop producing output and flush.
- `PeerGone` means the connection closed, whether the peer disconnected,
  the transport failed, or shutdown started locally. It carries the
  transport error if one occurred, or `None` after a clean close.

Use [`Io::poll_read_pause`] if you also want to stop accepting transport input
while waiting. The pause is cooperative. Decoding, accessing the application
read buffer, or explicitly requesting more input resumes reads. Use it while
a dispatcher waits on readiness, not to disable the read half permanently.

A codec-driven service receives the same conditions as [`RecvError`] from
[`Io::poll_recv`]. It also receives decoder failures. Unlike the status
update, `RecvError::PeerGone` reports a stream truncated mid-frame. If clean
peer EOF leaves undecodable bytes, it carries an
`io::ErrorKind::UnexpectedEof` error rather than `None`.

If you only need to wake when the connection goes away, await the [`Waiter`]
returned by [`IoRef::on_disconnect`]. To check whether closing has started,
use [`IoRef::is_active`], which becomes false at that point.
[`IoRef::is_closed`] becomes true only after the backend releases the
transport and finishes teardown.

Clean peer EOF ends the read direction but leaves the write half open. Your
service can finish encoding and flushing its response before closing.

When you have finished the response, call [`IoRef::close`] to request
graceful shutdown and return immediately, or await [`Io::shutdown`] to drive
it to completion. Graceful shutdown gives both filters and the transport time
to finish, in two phases.

During the first phase, both directions stay open. Each filter can emit
closing data through [`FilterLayer::shutdown`], such as TLS `close_notify` or
a WebSocket close frame. Reads continue normally, so the peer's closing data
still reaches the filters. A read failure does not abort shutdown, but it
means the filter handshake cannot finish.

During the second phase, the transport drains remaining output into the
connection, then closes both directions. The read side pauses because the
filters have finished and cannot use more input. The transport discards any
data left in the receive queue just before closing. The application receives
no more input in this phase.

One deadline covers both phases. If the filters finish early, the transport
gets the remaining time to drain output.

If the deadline expires while filters are pending, ntex ends the filter phase
and starts transport shutdown with the already-expired deadline. When the
transport observes it, it discards undrained output. `Io::shutdown` reports
the timeout error after transport teardown.

[`IoRef::terminate`] skips graceful shutdown and drops the connection without
flushing pending output.

Dropping the owning `Io` does not replace awaiting shutdown. If no output
would be lost, dropping it starts a normal close. If dropping the filter stack
prevents buffered output from being delivered, ntex aborts the transport.
The peer then cannot mistake a truncated stream for a complete one. Call
`shutdown().await` when delivery and a clean close matter.

[`FilterLayer::shutdown`]: https://docs.ntex.rs/api/ntex/io/trait.FilterLayer.html#method.shutdown
[`Io::poll_recv`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.poll_recv
[`Io::poll_read_pause`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.poll_read_pause
[`Io::poll_status_update`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.poll_status_update
[`Io::shutdown`]: https://docs.ntex.rs/api/ntex/io/struct.Io.html#method.shutdown
[`IoRef::close`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.close
[`IoRef::is_active`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.is_active
[`IoRef::is_closed`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.is_closed
[`IoRef::on_disconnect`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.on_disconnect
[`IoRef::terminate`]: https://docs.ntex.rs/api/ntex/io/struct.IoRef.html#method.terminate
[`IoStatusUpdate`]: https://docs.ntex.rs/api/ntex/io/enum.IoStatusUpdate.html
[`RecvError`]: https://docs.ntex.rs/api/ntex/io/enum.RecvError.html
[`Waiter`]: https://docs.ntex.rs/api/ntex/io/struct.Waiter.html

## Testing

Use [`IoTest`] to test a codec, filter, or protocol service without opening
sockets. It creates two connected in-memory transports. Each endpoint
implements `IoStream`, so you can wrap it in `Io`.

Writing to one endpoint supplies input to the other. The `read()` method
collects bytes written back by the peer:

```rust
use ntex::codec::BytesCodec;
use ntex::io::{Io, testing::IoTest};
use ntex::util::Bytes;

#[ntex::test]
async fn protocol_io() {
    let (client, server) = IoTest::create();

    // Allow the server transport to write to the client.
    client.remote_buffer_cap(1024);

    let io = Io::from(server);

    client.write(b"request");
    let request = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(request, Bytes::from_static(b"request"));

    io.send(Bytes::from_static(b"response"), &BytesCodec)
        .await
        .unwrap();
    assert_eq!(client.read().await.unwrap(), b"response"[..]);
}
```

You can also force pending reads or inject read and write errors. Close either
side, or constrain write capacity to exercise backpressure. For
transport-query tests, attach a `PeerAddr` value.

[`IoTest`]: https://docs.ntex.rs/api/ntex/io/testing/struct.IoTest.html
