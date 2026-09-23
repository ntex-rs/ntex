# I/O Abstraction Layer

ntex provides an I/O abstraction layer that keeps protocol and service code
independent of a specific runtime or reactor implementation, such as Tokio,
Compio, or Neon.

In addition to presenting a consistent interface across these backends, the
I/O layer provides the building blocks needed to manage buffered reads and
writes, backpressure, timeouts, and graceful shutdown.

## Socket abstractions

ntex achieves independence from the underlying socket implementation through
dependency inversion. The I/O subsystem does not call Tokio, Compio, or Neon
socket APIs directly. Instead, a runtime adapter moves bytes between its socket
and the buffers owned by [`Io`]. One way to picture it: the read half carries
bytes from the transport to the application and the write half carries them
back, with `Io` sitting between the two. Each half is an in-memory buffer that
`Io` owns, which is what lets it apply backpressure and run filter
transformations on bytes as they pass through.

An active connection has three cooperating execution responsibilities:

1. A protocol or dispatcher task consumes decoded input and queues encoded
   output through `Io` or [`IoRef`]. This task typically runs the protocol
   service and, indirectly, application code.
2. A transport read task waits for read readiness, reads bytes from the socket,
   and submits them to the I/O subsystem.
3. A transport write task takes queued bytes from the I/O subsystem and writes
   them to the socket.

The runtime adapter decides how to schedule these responsibilities. The
built-in adapters typically use cooperating read and write tasks, but this is
an implementation detail rather than part of the `IoStream` contract.

The read task cooperates with ntex backpressure. It reads only while
[`IoContext::poll_read_ready`] permits more input. It takes a buffer through
[`IoContext::take_read_buf`] and releases it with the result through
[`IoContext::release_read_buf`]. This allows the I/O
subsystem to process filters, wake the dispatcher, and pause further reads when
the configured high-water mark is reached.

A backend that reads synchronously, without holding the buffer across an await,
should use [`IoContext::with_read_buf`] instead. It combines the two steps and
borrows the read buffer in place, so no buffer is detached and appended back.

The write task follows the same pattern. It waits for
[`IoContext::poll_write_ready`], obtains queued data with
[`IoContext::with_write_dst`], and reports through
[`IoContext::update_write_status`] how many bytes reached the peer. ntex can
then apply write backpressure, resume waiting services, and coordinate
graceful shutdown.

A transport does not have to write the bytes before it reports. Completion
based backends take ownership of whole pages and keep them until the operation
completes. Those bytes leave the write buffer but have not reached the peer, so
ntex counts them as outstanding output until the transport either reports them
as written or returns them to the buffer. Flush completion, write backpressure,
and the shutdown drain all account for them.

Both methods return an [`IoTaskStatus`]. `Io` means work remains, not that the
transport is ready, so a task re-arms transport readiness before its next
operation. On the write side it is returned whenever output is still buffered,
including after an attempt that made no progress. Output the transport already
owns does not keep the task running, because its completion wakes the task
again.

Both readiness methods resolve to a [`Readiness`] value rather than a plain
ready signal. `Readiness::Ready` allows the task to perform its next operation.
`Readiness::Close` means the connection must be taken down: the task closes
both directions and releases the socket, cancelling anything still in flight.
For a socket this is `shutdown(SHUT_RDWR)` followed by `close()`. Whatever the
peer still has in the receive queue should be discarded first, because closing
a socket with unread input aborts the connection with an RST and loses the
output that was just drained.

`Readiness::Close` covers a graceful shutdown and an immediate termination
alike, and a task does not need to tell them apart. On the graceful path the
I/O subsystem reports it only once buffered output has been drained, so in
either case nothing is left to flush.

Teardown is reported back through two methods. A transport that fails or
decides to abort calls [`IoContext::stop`] with the error, which terminates the
connection without draining pending work. Once transport teardown has actually
finished, and regardless of which side initiated it, the task calls
[`IoContext::stopped`]. That is what completes [`Io::shutdown`], which resolves
only after the backend reports the connection stopped, not merely when filter
shutdown and flushing are done.

Socket types integrate with the I/O subsystem by implementing [`IoStream`].
Its `start()` method receives an [`IoContext`], starts the transport-specific
tasks, and returns a [`Handle`] used to control or query the transport.
A simplified Tokio-style adapter looks like this:

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
            Readiness::Close => break,
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
    loop {
        // resolves `IoContext::poll_write_ready`
        match wait_for_write_readiness(&ctx).await {
            Readiness::Ready => {}
            Readiness::Close => break,
        }

        let result = ctx.with_write_dst(|buf| {
            write_to_socket(&socket, buf)
        });

        // `result` carries the number of bytes that reached the peer
        match ctx.update_write_status(result) {
            IoTaskStatus::Io => {}
            IoTaskStatus::Pause => wait_for_queued_output(&ctx).await,
            IoTaskStatus::Stop => break,
        }
    }

    // close both directions, then report teardown as complete
    shutdown_socket(&socket).await;
    ctx.stopped(None);
}
```

The example is illustrative; runtime adapters use their native readiness and
buffer APIs. The important boundary is that only the adapter accesses the
socket, while the application and protocol layers interact with `Io` and
`IoRef`. Buffer limits and read/write backpressure are therefore applied
consistently across all supported runtimes.

An `Io` object is normally passed to a protocol service such as
[`ntex::http::HttpService`] or `ntex_mqtt::Server`. The protocol service
decodes incoming bytes into protocol messages and encodes its responses back
into the I/O write buffer without depending on the concrete socket type.

[`Handle`]: https://docs.rs/ntex/latest/ntex/io/trait.Handle.html
[`Io`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html
[`IoContext`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html
[`IoContext::poll_read_ready`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.poll_read_ready
[`IoContext::poll_write_ready`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.poll_write_ready
[`IoContext::stop`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.stop
[`IoContext::stopped`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.stopped
[`IoContext::with_read_buf`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.with_read_buf
[`IoContext::take_read_buf`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.take_read_buf
[`IoContext::release_read_buf`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.release_read_buf
[`IoContext::update_write_status`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.update_write_status
[`IoContext::with_write_dst`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.with_write_dst
[`IoRef`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html
[`IoStream`]: https://docs.rs/ntex/latest/ntex/io/trait.IoStream.html
[`IoTaskStatus`]: https://docs.rs/ntex/latest/ntex/io/enum.IoTaskStatus.html
[`Readiness`]: https://docs.rs/ntex/latest/ntex/io/enum.Readiness.html
[`ntex::http::HttpService`]: https://docs.rs/ntex/latest/ntex/http/struct.HttpService.html

The runtime-specific implementations are provided by the [`ntex-net`] crate,
which supports Tokio, Compio, and Neon backends.

[`ntex-net`]: https://docs.rs/ntex-net/

### Transport handles and metadata

Because the underlying socket is hidden behind the I/O abstraction, code using
`Io` cannot access transport-specific methods directly. The [`Handle`] returned
by `IoStream::start()` provides the bridge to the underlying transport. It can
resume the transport write operation on request and expose transport-specific
metadata through typed queries.

Each backend decides which query types it supports. For example, the built-in
network backends expose the remote socket address as [`PeerAddr`].
[`IoRef::query`] is also available on `Io` through dereferencing and returns a
[`QueryItem`], from which the value can be retrieved with `get()`:

```rust
use ntex::io::{Io, types::PeerAddr};

fn log_peer_addr(io: &Io) {
    if let Some(addr) = io.query::<PeerAddr>().get() {
        println!("Peer address {:?}", addr.into_inner());
    }
}
```

Queries travel through the filter stack before reaching the transport handle,
so filters may also expose their own typed metadata.

[`IoRef::query`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.query
[`PeerAddr`]: https://docs.rs/ntex/latest/ntex/io/types/struct.PeerAddr.html
[`QueryItem`]: https://docs.rs/ntex/latest/ntex/io/types/struct.QueryItem.html

## Configuration and timeouts

I/O settings are stored in [`IoConfig`] and are normally added to a
[`SharedCfg`]. `Io::new()` retrieves the `IoConfig` from that shared
configuration, using the default settings when it is not present.

```rust
use ntex::{
    SharedCfg,
    io::IoConfig,
    time::{Millis, Seconds},
};

let cfg = SharedCfg::new("my-protocol")
    .add(
        IoConfig::new()
            .set_connect_timeout(Millis(5_000))
            .set_keepalive_timeout(Seconds(30))
            .set_shutdown_timeout(Seconds(2))
            .set_frame_read_rate(Seconds(2), Seconds(10), 1_024)
            .set_read_buf(32 * 1024, 1024, 16)
            .set_write_buf(32 * 1024)
            .set_write_buf_threshold(8 * 1024),
    )
    .build();
```

These settings are used by different parts of the stack:

- The connection timeout is applied by `ntex-net` while resolving and opening
  an outgoing connection.
- The keep-alive timeout and frame read-rate limits are interpreted by
  protocol dispatchers. A frame read-rate limit protects a decoder from peers
  that send one incomplete frame too slowly. It also applies to the first
  frame of a new connection, so a peer that connects and stays silent is
  closed once the limit expires.
- The graceful-shutdown timeout bounds both phases of shutdown together: the
  filter shutdown and the transport drain of pending output. It cannot be
  disabled; a zero timeout is rejected.
- The read and write high-water marks enable backpressure. Write backpressure
  is released after outstanding output falls to half its high-water mark,
  counting both buffered output and output a transport has taken ownership of
  but not yet written to the peer.
- The read low-water mark controls how much free capacity is reserved before
  another socket read. The cache-size argument limits the number of eligible
  read buffers retained in the per-thread cache. Neither applies to output,
  which is held in [`BytePages`] rather than cached read buffers, so the write
  setting takes only a high-water mark.
- The write page size controls newly allocated [`BytePages`], while the write
  threshold controls when supported transports attempt an early direct write.

Connection and keep-alive timeouts are disabled by default. Frame read-rate
limits are also disabled. The default graceful-shutdown timeout is one
second, and the default read and write high-water marks are approximately
16 KiB.

An established connection can switch to another shared configuration with
[`Io::set_config`]. This is useful when a protocol upgrade changes timeout or
buffer requirements. The method is `unsafe`: replacing the configuration may
release the allocation that [`IoRef::cfg`] hands out, so no reference obtained
from it may be live across the call or used afterwards.

[`Io::set_config`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.set_config
[`IoRef::cfg`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.cfg
[`SharedCfg`]: https://docs.rs/ntex/latest/ntex/struct.SharedCfg.html

## Filter subsystem

Applications often need to transform a byte stream before a protocol service
processes it. TLS must decrypt incoming records and encrypt outgoing data. A
protocol may also be tunneled through another framing layer, such as MQTT over
WebSocket.

ntex implements these transformations with [`FilterLayer`]. A filter operates
on in-memory byte buffers: [`FilterLayer::process_read_buf`] transforms data
received from the next inner layer, while [`FilterLayer::process_write_buf`]
transforms data queued by the application before passing it toward the
transport. Filters do not perform socket I/O themselves, so the same filter
can be used with any supported runtime backend.

Filters are composable. [`Io::add_filter`] adds a layer and allocates the
intermediate read and write buffers that separate it from adjacent layers. For
example, an MQTT service can receive its byte stream through either of these
stacks:

```text
socket <-> TLS <-> MQTT

socket <-> TLS <-> WebSocket <-> MQTT
```

On reads, bytes move from the socket through the inner filters toward the
application. On writes, they move in the opposite direction. In the second
stack, the WebSocket filter removes and creates WebSocket framing, while the
TLS filter decrypts and encrypts the resulting byte stream. The MQTT service
still reads and writes MQTT bytes and does not need to know which transport
filters are installed below it.

Filters may maintain protocol state, expose typed metadata through `query()`,
emit output while processing input, and participate in graceful shutdown. For
example, a TLS filter can expose the negotiated protocol or peer certificate,
and a WebSocket filter can generate a close frame during shutdown. Output a
filter writes while processing a read is pushed toward the transport as soon as
that processing finishes, without waiting for the application to write.

[`FilterLayer`]: https://docs.rs/ntex/latest/ntex/io/trait.FilterLayer.html
[`FilterLayer::process_read_buf`]: https://docs.rs/ntex/latest/ntex/io/trait.FilterLayer.html#tymethod.process_read_buf
[`FilterLayer::process_write_buf`]: https://docs.rs/ntex/latest/ntex/io/trait.FilterLayer.html#tymethod.process_write_buf
[`Io::add_filter`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.add_filter

Most byte transformations only need [`FilterLayer`]. Lower-level concerns that
must observe or control the entire filter chain can instead wrap the current
chain with [`Filter`] by using [`Io::map_filter`].

In addition to processing buffers, queries, and shutdown, `Filter` participates
in read and write readiness decisions. A wrapper can therefore delay readiness
to implement custom throttling and wake the I/O tasks when work may resume. It
can also observe buffer processing for metrics or accounting without changing
the byte stream.

A custom `Filter` normally stores the filter it wraps and delegates every
operation it does not intentionally override. ntex provides forwarding macros
for readiness, queries, and shutdown to make this pattern less error-prone.

[`Filter`]: https://docs.rs/ntex/latest/ntex/io/trait.Filter.html
[`Io::map_filter`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.map_filter

### Typed versus erased filter stacks

The filter stack is encoded in the type parameter of [`Io`]. A new connection
starts as `Io<Base>`, using the [`Base`] filter. Calling `add_filter(layer)`
consumes the current value and returns `Io<Layer<U, F>>`, where the [`Layer`]
marker pairs the new outer layer `U` with the previous stack `F`. Keeping this
concrete type provides static dispatch and allows [`Io::filter`] to return the
concrete outer filter.

```rust,ignore
let io: Io<Base> = create_io();
let io: Io<Layer<MyFilter, Base>> = io.add_filter(MyFilter::new());
```

At service boundaries, different connections may have different concrete
filter stacks. [`Io::seal`] erases the stack type and returns `Io<Sealed>`,
using the [`Sealed`] marker, while [`Io::boxed`] returns the [`IoBoxed`]
convenience wrapper. Both operations consume the original `Io` value and retain
the same connection state and filter behavior behind a dynamically dispatched
`Filter`.

```rust,ignore
let io: IoBoxed = io.boxed();
start_protocol(io);
```

Additional typed layers can still be added to a sealed stream and the result
can be erased again when necessary. Type erasure is therefore normally
performed at the boundary where a protocol or service needs one uniform I/O
type, rather than while constructing the filter stack.

[`Base`]: https://docs.rs/ntex/latest/ntex/io/struct.Base.html
[`Io::boxed`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.boxed
[`Io::filter`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.filter
[`Io::seal`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.seal
[`IoBoxed`]: https://docs.rs/ntex/latest/ntex/io/struct.IoBoxed.html
[`Layer`]: https://docs.rs/ntex/latest/ntex/io/struct.Layer.html
[`Sealed`]: https://docs.rs/ntex/latest/ntex/io/struct.Sealed.html

## Read/write streams

Incoming and outgoing bytes use separate buffer paths.

### Reading

The transport adapter places bytes read from the socket into a [`BytesMut`].
The bytes pass through the filter chain and arrive in the application-facing
read buffer, where a codec or protocol service can inspect and consume them.

`BytesMut` is a contiguous, growable buffer. A decoder can split immutable
[`Bytes`] values from it in constant time without copying the payload. This is
useful when a decoded message must retain part of the input after the decoder
continues processing later data.

ntex reuses eligible read buffers through a per-thread cache and retains spare
capacity where possible. Before another socket read, the adapter obtains a
buffer from `IoContext`. ntex ensures that a reused buffer has at least the
configured low-water mark available. If the retained capacity is
insufficient, the buffer grows and may allocate additional storage.

Read backpressure is based on the size of the application-facing read buffer.
When it reaches the configured high-water mark, ntex pauses the transport read
task. Consuming input through [`IoRef::decode`], [`IoRef::with_buf`],
[`IoRef::with_read_src`] or [`IoRef::with_read_dst`] wakes the read task once
the buffered input falls to half that mark.
[`Io::recv`] and [`Io::read_exact`] wait for more input, so they release
backpressure regardless of how much is still buffered.

### Writing

Application output is queued in [`BytePages`], a growable collection of
byte pages. Internally allocated pages use the size configured by [`IoConfig`].
Owned buffers passed to [`IoRef::encode_bytes`] can also become pages directly,
avoiding a copy when their storage can be retained. Write filters consume the
pages in order, transform their contents, and place the result into the next
buffer toward the transport.

[`IoRef::encode`], [`IoRef::encode_slice`], and [`IoRef::encode_bytes`] queue
output but do not wait for every byte to reach the socket. Queueing makes the
data available to the transport write task and schedules that task when it
needs to be resumed. On transports that support direct writes, the configured
write threshold can trigger an earlier write while the application is still
producing output, reducing latency for large responses.

Use [`Io::flush`] to wait for write progress. `flush(false)` returns
immediately while the outstanding output is below the high-water mark. If the
high-water mark has been reached, it waits until the outstanding output falls
to half that mark. `flush(true)` waits until all queued data has reached the
peer, including data a transport has taken ownership of but not yet written.
[`Io::send`] combines codec encoding with a full flush.

This separation allows codecs and application services to work with bytes
without depending on socket readiness, while the I/O subsystem consistently
enforces buffer limits and backpressure.

[`BytePages`]: https://docs.rs/ntex/latest/ntex/util/struct.BytePages.html
[`Bytes`]: https://docs.rs/ntex/latest/ntex/util/struct.Bytes.html
[`BytesMut`]: https://docs.rs/ntex/latest/ntex/util/struct.BytesMut.html
[`Io::flush`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.flush
[`Io::read_exact`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.read_exact
[`Io::recv`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.recv
[`Io::send`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.send
[`IoConfig`]: https://docs.rs/ntex/latest/ntex/io/struct.IoConfig.html
[`IoRef::decode`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.decode
[`IoRef::encode`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.encode
[`IoRef::encode_bytes`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.encode_bytes
[`IoRef::encode_slice`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.encode_slice
[`IoRef::with_read_dst`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.with_read_dst
[`IoRef::with_read_src`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.with_read_src
[`IoRef::with_buf`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.with_buf

## Connection lifecycle and shutdown

A connection stays usable until the application closes it, the peer
disconnects, or the transport reports an error.

A service that is waiting on something other than input, such as an unready
dependency or a slow response, still needs to notice that the connection
requires attention. [`Io::poll_status_update`] reports the next status as an
[`IoStatusUpdate`] value:

- `KeepAlive` when the configured keep-alive timeout has expired.
- `WriteBackpressure` when outstanding output has reached the write
  high-water mark, so the producer should stop and flush.
- `PeerGone` once the connection has closed, whether the peer disconnected,
  the transport failed, or the shutdown was started locally. It carries the
  transport error if one occurred, and `None` after a clean close.

The same conditions reach a codec-driven service as [`RecvError`] from
[`Io::poll_recv`], which additionally reports decoder failures. Code that only
needs to be woken when the connection goes away can await the [`OnDisconnect`]
future returned by [`IoRef::on_disconnect`], and [`IoRef::is_closed`] reports
whether shutdown has already started.

A clean EOF from the peer ends the read direction but leaves the write half
open, so a service can still finish encoding and flushing its response before
closing.

[`IoRef::close`] requests a graceful shutdown and returns immediately.
[`Io::shutdown`] drives that shutdown to completion, in two phases.

In the first phase both directions stay open and every filter gets a chance to
emit its own closing data through [`FilterLayer::shutdown`], such as a TLS
`close_notify` or a WebSocket close frame. Reads keep running and are processed
normally, so closing data sent by the peer still reaches the filters. A read
failure does not abort the shutdown; it only means the filter handshake cannot
finish.

The second phase belongs to the transport. It drains the remaining output into
the connection and then closes both directions. The read side is paused: the
filters are done, so no further input can be used, and the transport discards
whatever is left in the receive queue just before it closes.
Input is no longer delivered to the application in this phase.

A single graceful-shutdown timeout bounds both phases; if it elapses the
connection is terminated and any undrained output is discarded.
[`IoRef::terminate`] skips the process entirely and drops the connection
without flushing pending output.

Either way the transport observes the end of the connection as
[`Readiness::Close`] and reports its own teardown through
[`IoContext::stopped`], which is what allows `Io::shutdown` to resolve.

[`FilterLayer::shutdown`]: https://docs.rs/ntex/latest/ntex/io/trait.FilterLayer.html#method.shutdown
[`Io::poll_recv`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.poll_recv
[`Io::poll_status_update`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.poll_status_update
[`Io::shutdown`]: https://docs.rs/ntex/latest/ntex/io/struct.Io.html#method.shutdown
[`IoRef::close`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.close
[`IoRef::is_closed`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.is_closed
[`IoRef::on_disconnect`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.on_disconnect
[`IoRef::terminate`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html#method.terminate
[`IoStatusUpdate`]: https://docs.rs/ntex/latest/ntex/io/enum.IoStatusUpdate.html
[`OnDisconnect`]: https://docs.rs/ntex/latest/ntex/io/struct.OnDisconnect.html
[`Readiness::Close`]: https://docs.rs/ntex/latest/ntex/io/enum.Readiness.html#variant.Close
[`RecvError`]: https://docs.rs/ntex/latest/ntex/io/enum.RecvError.html

## Testing

[`IoTest`] provides a pair of interconnected in-memory transports for testing
codecs, filters, and protocol services without opening sockets. Each endpoint
implements `IoStream` and can be wrapped in `Io`. Writing to one `IoTest`
endpoint supplies input to the other endpoint, while `read()` collects bytes
written back by the peer.

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

Tests can also force pending reads, inject read or write errors, close either
side, constrain write capacity to exercise backpressure, and attach a
`PeerAddr` value for transport-query tests.

[`IoTest`]: https://docs.rs/ntex/latest/ntex/io/testing/struct.IoTest.html
