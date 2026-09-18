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
and the buffers owned by [`Io`].

An active connection has three cooperating execution responsibilities:

1. A protocol or dispatcher task consumes decoded input and queues encoded
   output through `Io` or [`IoRef`]. This task typically runs the protocol
   service and, indirectly, application code.
2. A transport read task waits for read readiness, reads bytes from the socket,
   and submits them to the I/O subsystem.
3. A transport write task takes queued bytes from the I/O subsystem and writes
   them to the socket.

An adapter may run these responsibilities as separate tasks or combine them.
For example, the Tokio adapter uses separate read and write tasks, while the
Compio adapter drives both directions from one transport task.

The read task cooperates with ntex backpressure. It reads only while
[`IoContext::poll_read_ready`] permits more input. After reading, it returns the
buffer and result through [`IoContext::update_read_status`]. This allows the I/O
subsystem to process filters, wake the dispatcher, and pause further reads when
the configured high-water mark is reached.

The write task follows the same pattern. It waits for
[`IoContext::poll_write_ready`], obtains queued data with
[`IoContext::with_write_buf`], and reports progress through
[`IoContext::update_write_status`]. ntex can then apply write backpressure,
resume waiting services, and coordinate graceful shutdown.

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
        wait_for_read_readiness(&ctx).await;

        let mut buf = ctx.get_read_buf();
        let result = read_from_socket(&socket, &mut buf);

        match ctx.update_read_status(buf, result) {
            IoTaskStatus::Io => {}
            IoTaskStatus::Pause => wait_for_read_resume(&ctx).await,
            IoTaskStatus::Stop => break,
        }
    }
}

async fn write_task(socket: Rc<TcpStream>, ctx: IoContext) {
    loop {
        wait_for_write_readiness(&ctx).await;

        let result = ctx.with_write_buf(|buf| {
            write_to_socket(&socket, buf)
        });

        match ctx.update_write_status(result) {
            IoTaskStatus::Io => {}
            IoTaskStatus::Pause => wait_for_queued_output(&ctx).await,
            IoTaskStatus::Stop => break,
        }
    }
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
[`IoContext::update_read_status`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.update_read_status
[`IoContext::update_write_status`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.update_write_status
[`IoContext::with_write_buf`]: https://docs.rs/ntex/latest/ntex/io/struct.IoContext.html#method.with_write_buf
[`IoRef`]: https://docs.rs/ntex/latest/ntex/io/struct.IoRef.html
[`IoStream`]: https://docs.rs/ntex/latest/ntex/io/trait.IoStream.html
[`ntex::http::HttpService`]: https://docs.rs/ntex/latest/ntex/http/struct.HttpService.html

The runtime-specific implementations are provided by the [`ntex-net`] crate,
which supports Tokio, Compio, and Neon backends.

[`ntex-net`]: https://docs.rs/ntex-net/

### Transport handles and metadata

Because the underlying socket is hidden behind the I/O abstraction, code using
`Io` cannot access transport-specific methods directly. The [`Handle`] returned
by `IoStream::start()` provides the bridge to the underlying transport. It can
respond to control notifications and expose transport-specific metadata through
typed queries.

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

## Filter
