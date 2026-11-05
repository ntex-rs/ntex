# MQTT with ntex-mqtt

[MQTT] is a lightweight publish/subscribe protocol. It is common in IoT,
telemetry and messaging systems, where many small devices keep long-lived
connections to a broker. [ntex-mqtt] brings MQTT to ntex. It provides both a
client and a server, and both speak the two versions of the protocol still in
use, MQTT 3.1.1 and MQTT 5.0. A single server can even accept clients of both
versions on the same port.

ntex-mqtt is built from the same pieces as the rest of ntex. Connections are
ntex I/O objects, handlers are ntex services, settings travel in a
[`SharedCfg`], and a server runs inside the ordinary
[ntex server](5-server.md). If you know how to write an ntex web service,
most of this chapter will feel familiar.

Add the crate next to ntex:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio"] }
ntex-mqtt = "9"
```

Here is a server that accepts both protocol versions and prints every message
it receives:

```rust,no_run
use ntex::SharedCfg;
use ntex_mqtt::{MqttServer, v3, v5};

#[derive(Debug)]
struct ServerError;

impl TryFrom<ServerError> for v5::PublishAck {
    type Error = ServerError;

    fn try_from(err: ServerError) -> Result<Self, Self::Error> {
        Err(err)
    }
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    ntex::server::build()
        .bind("mqtt", "127.0.0.1:1883", SharedCfg::default(), async |_| {
            MqttServer::new()
                .v3(
                    v3::MqttServer::new(async |publish: v3::Publish| {
                        println!("v3 publish to {}", publish.publish_topic());
                        Ok::<_, ServerError>(())
                    })
                    .build(async |connect: v3::Connect| {
                        Ok::<_, ServerError>(connect.ack((), false))
                    }),
                )
                .v5(
                    v5::MqttServer::new(async |publish: v5::Publish| {
                        println!("v5 publish to {}", publish.publish_topic());
                        Ok::<_, ServerError>(publish.ack())
                    })
                    .build(async |connect: v5::Connect| {
                        Ok::<_, ServerError>(connect.ack(()))
                    }),
                )
        })?
        .run()
        .await
}
```

Most server examples in this chapter use an error type like `ServerError`.
Its `TryFrom` implementation answers one question: can this error be turned
into a `PUBACK` that refuses the message, or must it close the connection?
Returning `Err` means "close". [Handling publishes](#handling-publishes)
shows the other choice.

The rest of the chapter explains how the crate is organized, how to
configure it, and then covers the client and the server in turn.

## How ntex-mqtt is organized

The two protocol versions live in two modules, `ntex_mqtt::v3` and
`ntex_mqtt::v5`. They have the same shape: each has a server builder
(`MqttServer`), a client connector (`client::MqttConnector`), a `Publish`
message, a `Router`, a `MqttSink` and a `codec` module with the packet types.
Code written for one version translates to the other almost line by line.
MQTT 5 adds reason codes, properties and a few new packets, so its types carry
more information.

The crate root holds what the versions share: the combined `MqttServer`, the
`MqttServiceConfig` settings, and the `Control` messages that report what
happens to a connection.

Every connection, on either side, is handled by up to three services:

- The **publish service** receives `PUBLISH` packets from the peer. This is
  where your application logic lives. On the client, this role is filled by
  router handlers and the protocol service.
- The **protocol service** receives the other packets that need an answer:
  `SUBSCRIBE`, `UNSUBSCRIBE`, `PINGREQ`, `DISCONNECT`, `PUBREL` and, in MQTT 5,
  `AUTH`. It returns the response to send back.
- The **control service** is told about events in the connection's life,
  such as write backpressure or the connection stopping, and why it stopped.

The protocol and control services have sensible defaults, so you only
replace them when you need to.

Two more types show up everywhere. A **sink** (`MqttSink`) is a cheap,
cloneable handle for sending packets to the peer: publishes, subscriptions,
and the final `DISCONNECT`. A **session** (`v3::Session<St>` or
`v5::Session<St>`) bundles the sink with your own per-connection state `St`.
It dereferences to `St`, and its `sink()` method returns the sink.

## Configuration

Protocol settings live in `MqttServiceConfig`. As with other ntex
configuration, you add it to a [`SharedCfg`], and every connection created
with that `SharedCfg`, client or server, picks it up:

```rust,no_run
use ntex::{SharedCfg, time::Seconds};
use ntex_mqtt::{MqttServiceConfig, QoS};

fn config() -> SharedCfg {
    SharedCfg::new("MQTT")
        .add(
            MqttServiceConfig::new()
                .set_max_qos(QoS::ExactlyOnce)
                .set_max_size(1024 * 1024)
                .set_max_receive(32)
                .set_connect_timeout(Seconds(10)),
        )
        .into()
}
```

MQTT connections are ordinary ntex I/O objects, so an [`IoConfig`] in the
same `SharedCfg` controls their buffers, write backpressure and timeouts, as
described in the [I/O chapter](7-io.md).

The settings fall into three groups. The first covers what a peer may send
you:

| Setting | Default | Meaning |
|---|---|---|
| `set_max_qos` | `AtLeastOnce` | Highest QoS a peer may publish with. A higher QoS is a protocol violation and closes the connection. An MQTT 5 server announces it in `CONNACK` |
| `set_max_size` | 256 KiB | Largest packet the peer may send; 0 means the protocol maximum of 256 MiB. An MQTT 5 server announces it in `CONNACK` |
| `set_max_topic_alias` | 32 | Topic aliases an MQTT 5 client may use when publishing to the server |
| `set_check_subs_availability` | `true` | MQTT 5 server only. Close the connection when a client subscribes to a shared or wildcard filter that `CONNACK` declared unavailable. When disabled, the protocol service decides |

The second controls flow control, explained in detail
[below](#flow-control):

| Setting | Default | Meaning |
|---|---|---|
| `set_max_receive` | 16 | Incoming QoS 1 and 2 publishes being handled at once. MQTT 5 announces it to the peer as Receive Maximum; 0 means 65535. For MQTT 3.1.1 it queues extra publishes; 0 disables the limit |
| `set_max_receive_size` | 64 KiB | Total size of incoming publishes being handled. When it is reached, the connection stops reading |
| `set_max_queue` | 64 | Responses waiting to be sent. Acks go out in the order the publishes arrived; when the queue is full, the connection stops reading |
| `set_max_send` | 16 | Outgoing QoS 1 and 2 publishes waiting for an ack. For MQTT 5 the peer's Receive Maximum can lower it |
| `set_min_chunk_size` | 32 KiB | How much of a large payload to collect before handing a chunk to your handler |
| `set_max_payload_buffer_size` | 32 KiB | Unread payload to buffer for a streaming publish before the connection stops reading |
| `set_handle_qos_after_disconnect` | `None` | Highest QoS of received publishes that are still handed to the publish service after the connection starts closing. By default they are dropped |

The third sets timeouts:

| Setting | Default | Meaning |
|---|---|---|
| `protocol_version_timeout` | 5 s | Combined server only. Time a client has to send enough of `CONNECT` to reveal its protocol version |
| `set_connect_timeout` | 5 s | Server only. Time a client has to send its `CONNECT` packet |
| `set_handshake_timeout` | disabled | Client only. Time allowed for connecting, sending `CONNECT` and receiving `CONNACK` |

Keep-alive is not part of `MqttServiceConfig`. A client chooses it in its
`CONNECT` packet, and a server may override it while accepting the
connection.

## The client

The client is a service that opens a connection, performs the MQTT handshake
and returns a `Client`. The examples here use MQTT 5. The MQTT 3.1.1 client in
`v3::client` works the same way, and the differences are noted as they come
up.

### Connecting

`v5::client::MqttConnector` is an ntex service. Wrap it in a [`Pipeline`]
together with the `SharedCfg` it should use, and call it with a
`v5::client::Connect` that describes the connection:

```rust,no_run
use ntex::{Pipeline, SharedCfg, time::Seconds};
use ntex_mqtt::v5;

#[ntex::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let connector = Pipeline::new(SharedCfg::default(), v5::client::MqttConnector::new());

    let client = connector
        .call(
            v5::client::Connect::new("127.0.0.1:1883")
                .client_id("sensor-17")
                .keep_alive(Seconds(30))
                .max_packet_size(64 * 1024)
                .max_receive(32),
        )
        .await?;

    println!("session present: {}", client.session_present());
    Ok(())
}
```

`Connect` builds the `CONNECT` packet. Besides the address, it sets:

- `client_id`, the identity the server uses to find your session;
- `keep_alive`, how often the client pings an idle connection. It is disabled
  by default;
- `clean_start`, which asks the server to discard any stored session
  (`clean_session` in MQTT 3.1.1);
- `last_will`, a message the server publishes if the client disappears
  without disconnecting;
- `username` and `password`, and in MQTT 5 `auth` for enhanced
  authentication;
- `max_packet_size` and `max_receive`, the client's own limits, announced to
  the server (MQTT 5 only);
- `properties` and `packet`, for direct access to the rest of the packet.

The connector returns a `Client` once the server accepts the connection. If
the server rejects it, the error carries the server's `CONNACK`, including its
reason code.

The server's `CONNACK` also shapes the connection. Its Receive Maximum limits
how many publishes the client may have unacknowledged, its Maximum Packet Size
limits what the client may send, and a Server Keep Alive replaces the client's
own keep-alive. `client.packet()` gives access to the full `CONNACK`.

### Secure connections

By default the connector opens a plain TCP connection. To use TLS, give it a
different transport connector with `connector()`. Any ntex connector works,
for example openssl:

```rust,no_run
use ntex::{Pipeline, SharedCfg, connect::openssl::SslConnector};
use ntex_mqtt::v5;
use openssl::ssl;

#[ntex::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut tls = ssl::SslConnector::builder(ssl::SslMethod::tls())?;
    tls.set_alpn_protos(b"\x04mqtt")?;

    let connector = Pipeline::new(
        SharedCfg::default(),
        v5::client::MqttConnector::new().connector(SslConnector::new(tls.build())),
    );
    let client = connector
        .call(v5::client::Connect::new("broker.example.com:8883").client_id("sensor-17"))
        .await?;
    # drop(client);
    Ok(())
}
```

`ntex_mqtt::TLS_PORT` (8883) and `TCP_PORT` (1883) hold the standard ports.
The same mechanism carries MQTT over WebSockets; the `mqtt-ws-client`
example in the ntex-mqtt repository shows how, and the
[MQTT over WebSocket example] has a complete client and server.

### Running the connection

A `Client` does nothing on its own. Something has to read packets from the
server, answer pings and deliver acks to the publishes you are waiting on.
That is the job of the client's dispatcher, which you start by consuming the
`Client`:

- `start_default()` runs the connection with default handlers. Any publish
  from the server closes the connection, so this suits a client that only
  sends.
- `start(service)` runs it with your own protocol service, which sees every
  publish and every other packet the server sends.
- `start_with_control(service, control)` also replaces the control service.

Each of these returns a future that completes when the connection closes.
Take the sink first, then spawn the dispatcher on the current thread:

```rust,no_run
# use ntex::{Pipeline, SharedCfg};
# use ntex_mqtt::v5;
# #[ntex::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let connector = Pipeline::new(SharedCfg::default(), v5::client::MqttConnector::new());
let client = connector.call(v5::client::Connect::new("127.0.0.1:1883")).await?;
let sink = client.sink();
ntex::rt::spawn(client.start_default());

let ack = sink.publish("sensors/17/temperature").send_at_least_once("21.5".into()).await?;
println!("reason: {:?}", ack.reason_code);

sink.close();
# Ok(())
# }
```

The client sends keep-alive pings while the dispatcher runs. Acks are read by
the dispatcher too, so a publish that waits for its ack waits forever if the
dispatcher was never started.

### Publishing

`sink.publish(topic)` returns a `PublishBuilder`. Set the packet options you
need, then send the payload with the QoS you want:

| Method | QoS | Completes when | Returns |
|---|---|---|---|
| `send_at_most_once` | 0 | The packet is written | `()` |
| `send_at_least_once` | 1 | The server sends `PUBACK` | `PublishAck` (`()` in MQTT 3.1.1) |
| `send_exactly_once` | 2 | The server sends `PUBREC` | `PublishReceived` |

A QoS 1 publish completes with the server's `PUBACK`. In MQTT 5 that ack
carries a reason code, and the server can use it to refuse a message without
closing the connection, for example with `NotAuthorized` or `QuotaExceeded`.
Check `reason_code` when it matters. An error from the call itself means the
publish could not be sent or acknowledged at all, usually because the
connection closed.

QoS 2 takes two round trips. `send_exactly_once` returns once the server has
received the message; call `release()` on the result to send `PUBREL` and
wait for `PUBCOMP`. If you drop the `PublishReceived` instead, ntex-mqtt
releases the publish for you.

MQTT 5 publishes can carry properties: user properties, content type,
message expiry, response topic and correlation data for request/response, and
so on. Set them with `properties()`:

```rust
# use ntex_mqtt::v5;
# async fn example(sink: v5::MqttSink) -> Result<(), v5::error::SendPacketError> {
let ack = sink
    .publish("sensors/17/temperature")
    .retain(true)
    .properties(|props| {
        props.content_type = Some("text/plain".into());
        props.message_expiry_interval = Some(60);
        props.user_properties.push(("unit".into(), "celsius".into()));
    })
    .send_at_least_once("21.5".into())
    .await?;
# drop(ack);
# Ok(())
# }
```

Packet ids are assigned automatically. You can set one yourself with
`packet_id()`, but do not mix both styles on one connection, or ids will
collide.

### Publishing without waiting

Awaiting each ack limits a task to one message per round trip. When you want
to keep many publishes in flight from one place, register an ack callback and
send with `send_at_least_once_no_block()`:

```rust
# use ntex_mqtt::v5;
# fn example(sink: v5::MqttSink) -> Result<(), v5::error::SendPacketError> {
sink.publish_ack_cb(|ack, disconnected| {
    if disconnected {
        println!("publish {} was not acknowledged", ack.packet_id);
    }
});

if sink.is_ready() {
    sink.publish("events").send_at_least_once_no_block("started".into())?;
}
# Ok(())
# }
```

The callback runs for every ack, in the order they arrive. When the
connection closes, it runs once more for each publish still in flight, with
`disconnected` set to `true`. It runs while the sink is borrowed, so it must
not call any sink method; hand the work to a channel or a task instead.

`send_at_least_once_no_block()` panics if the sink is not ready, so always
check `is_ready()` first, or wait for `ready()` as described below.

### Streaming large payloads

A publish normally carries its whole payload in one `Bytes`. For large
messages you can send the payload in pieces instead. Tell the builder the
total size, and you get a `StreamingPayload` to feed it through:

```rust
# use ntex::util::Bytes;
# use ntex_mqtt::v5;
# async fn example(sink: v5::MqttSink, chunks: Vec<Bytes>) -> Result<(), v5::error::SendPacketError> {
let size = chunks.iter().map(|c| c.len() as u32).sum();
let (ack, payload) = sink.publish("files/report.csv").stream_at_least_once(size);

let send = async {
    for chunk in chunks {
        payload.send(chunk).await?;
    }
    Ok::<_, v5::error::SendPacketError>(())
};
let (ack, sent) = ntex::util::join(ack, send).await;
sent?;
println!("reason: {:?}", ack?.reason_code);
# Ok(())
# }
```

The two halves must make progress together: `payload.send()` waits until the
publish header has been written, and the ack only arrives after the last
chunk. That is why the example joins them. The total of all chunks must match
the declared size exactly. `stream_at_most_once(size)` does the same for
QoS 0.

While a streaming publish is in progress, other publishes on the same
connection wait, because MQTT cannot interleave packets inside a payload.

Incoming publishes can be streamed as well; see
[Reading payloads](#reading-payloads).

### Flow control

The server tells the client how many QoS 1 and 2 publishes it may have
unacknowledged at once, its Receive Maximum. ntex-mqtt tracks this as
**send credit**. Each such publish takes one unit of credit when it is sent
and returns it when the ack arrives. When no credit is left, the next
`send_at_least_once()` or `send_exactly_once()` simply waits until an ack
frees some. Waiting publishes get credit in the order they asked for it. QoS 0
publishes do not use credit.

The sink also follows the connection's write backpressure. When the write
buffer grows past its high watermark (see the [I/O chapter](7-io.md)),
sends wait until it drains.

The sink exposes both so you can plan ahead:

- `credit()` returns the credit left;
- `is_ready()` is `true` when there is credit and no write backpressure;
- `ready().await` waits until the sink is ready, and returns `false` if the
  connection has closed.

A producer that should slow down rather than queue up can loop on
`ready()`:

```rust
# use ntex::util::Bytes;
# use ntex_mqtt::v5;
# async fn next_reading() -> Option<Bytes> { None }
# async fn example(sink: v5::MqttSink) -> Result<(), v5::error::SendPacketError> {
while let Some(reading) = next_reading().await {
    if !sink.ready().await {
        break; // connection closed
    }
    sink.publish("sensors/17").send_at_least_once_no_block(reading)?;
}
# Ok(())
# }
```

`ready()` does not reserve credit. When several tasks share a sink, another
task may use up the credit between your `ready()` and your send. An awaiting
send simply waits again, but `send_at_least_once_no_block()` would panic, so
in that case check `is_ready()` immediately before it.

In the other direction, the client tells the server its own Receive Maximum
with `Connect::max_receive()`, and the settings from
[Configuration](#configuration) limit how much incoming data it buffers.

### Subscribing

To receive messages, subscribe to one or more topic filters. In MQTT 5 each
filter comes with options:

```rust
# use ntex_mqtt::v5::{self, codec};
# async fn example(sink: v5::MqttSink) -> Result<(), v5::error::SendPacketError> {
let ack = sink
    .subscribe(None)
    .topic_filter(
        "commands/sensor-17/#".into(),
        codec::SubscriptionOptions {
            qos: codec::QoS::AtLeastOnce,
            no_local: false,
            retain_as_published: false,
            retain_handling: codec::RetainHandling::AtSubscribe,
        },
    )
    .send()
    .await?;

for status in &ack.status {
    println!("subscription: {status:?}");
}
# Ok(())
# }
```

The argument to `subscribe()` is an optional subscription identifier. The
server attaches it to every publish delivered through this subscription.
The `SUBACK` holds one status per filter, in order: the granted QoS, or a
reason the server refused that filter. `sink.unsubscribe()` works the same
way.

In MQTT 3.1.1, `subscribe()` takes no identifier and `topic_filter()` takes
just a QoS. The ack is a list of `SubscribeReturnCode`s.

Subscribing does not use send credit, but the server's answer is read by the
dispatcher, so the dispatcher must be running.

### Receiving publishes

Messages from your subscriptions arrive as publishes from the server. To
handle them, attach handlers to topics with `resource()`. This turns the
`Client` into a `ClientRouter`, which you start like a client:

```rust,no_run
use ntex::{Pipeline, SharedCfg};
use ntex_mqtt::v5;

#[derive(Debug)]
struct Error;

impl TryFrom<Error> for v5::PublishAck {
    type Error = Error;

    fn try_from(err: Error) -> Result<Self, Self::Error> {
        Err(err)
    }
}

async fn command(publish: v5::Publish) -> Result<v5::PublishAck, Error> {
    let id = publish.topic().get("id").unwrap_or_default().to_string();
    let payload = publish.read_all().await.map_err(|_| Error)?;
    println!("command for {id}: {payload:?}");
    Ok(publish.ack())
}

async fn status(publish: v5::Publish) -> Result<v5::PublishAck, Error> {
    println!("status: {:?}", publish.read_all().await);
    Ok(publish.ack())
}

#[ntex::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let connector = Pipeline::new(SharedCfg::default(), v5::client::MqttConnector::new());
    let client = connector.call(v5::client::Connect::new("127.0.0.1:1883")).await?;
    let sink = client.sink();

    let router = client
        .resource("commands/{id}/#", command)
        .resource("status", status);
    ntex::rt::spawn(router.start_default());

    sink.subscribe(None)
        .topic_filter("commands/sensor-17/#".into(), Default::default())
        .topic_filter("status".into(), Default::default())
        .send()
        .await?;

    ntex::time::sleep(ntex::time::Seconds(60)).await;
    Ok(())
}
```

Resource patterns match topic levels. A `{name}` segment matches one level
and captures it, which you read back with `publish.topic().get("name")`; a
trailing `#` matches the rest of the topic. A handler is any service that
takes a `Publish` and returns a `PublishAck`. It can also take
`&v5::Session<()>` as its first argument. In MQTT 3.1.1, handlers return
`()`.

The router acknowledges each publish with what the handler returns: `PUBACK`
for QoS 1, `PUBREC` for QoS 2. In MQTT 5 a handler can refuse a message with
`PublishAck::new(reason)`. A handler error closes the connection, unless the
error converts to a `PublishAck`. The `TryFrom` implementation above decides
that; returning `Ok(ack)` from it turns the error into a negative ack instead.

With `start_default()`, a publish whose topic matches no resource closes the
connection. To handle those, and every other packet from the server, use
`start()` with a protocol service.

### The protocol service

A client protocol service receives `v5::client::ProtocolMessage` values:

- `Publish`: a publish that no resource matched;
- `PublishRelease`: the server's `PUBREL` for a QoS 2 publish;
- `Ping`: a `PINGRESP` from the server;
- `Disconnect`: a `DISCONNECT` sent by the server, MQTT 5 only.

Each message has an `ack()` method that produces the normal response, and
`disconnect()` closes the connection instead. Unrouted publishes can be read
and acknowledged here:

```rust,no_run
# use ntex::{Pipeline, SharedCfg};
use ntex_mqtt::v5::{self, client::ProtocolMessage, client::ProtocolMessageAck, codec};

async fn protocol(msg: ProtocolMessage) -> Result<ProtocolMessageAck, ()> {
    Ok(match msg {
        ProtocolMessage::Publish(publish) => {
            println!("unrouted {:?}: {:?}", publish.packet().topic, publish.read_all().await);
            publish.ack(codec::PublishAckReason::Success)
        }
        msg => msg.ack(),
    })
}
# #[ntex::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let connector = Pipeline::new(SharedCfg::default(), v5::client::MqttConnector::new());
# let client = connector.call(v5::client::Connect::new("127.0.0.1:1883")).await?;

ntex::rt::spawn(client.start(protocol));
# Ok(())
# }
```

A `ClientRouter` accepts a protocol service the same way, with `start()`.

### The control service

The control service hears about the connection's life rather than its
packets. It receives `ntex_mqtt::Control` values:

- `WrBackpressure` reports when write backpressure starts and stops.
- `Stop(reason)` is sent once, as the connection shuts down. The reason is
  `Error` for an error from one of your services, `Protocol` when the peer
  broke the protocol or a keep-alive timed out, and `PeerGone` when the
  connection closed underneath.

The service returns an optional packet to send before closing, usually a
`DISCONNECT` with a reason code. The default control service does nothing.
On the client, set it with `start_with_control()`; the
[server section](#the-control-service-1) shows a full example.

### Closing

`sink.close()` sends `DISCONNECT` and closes the connection gracefully,
flushing what is already buffered. In MQTT 5, `close_with_reason()` sends a
`DISCONNECT` of your choice, for example with a session expiry interval or a
reason code. `force_close()` drops the connection immediately, discarding
buffered data and failing any publishes still in flight.

`sink.is_open()` turns `false` as soon as the connection starts closing,
whichever side closed it.

## The server

An MQTT server is a service factory that plugs into the
[ntex server](5-server.md). You can build one for a single protocol version
with `v3::MqttServer` or `v5::MqttServer`, or combine the two with
`ntex_mqtt::MqttServer`.

### A server for one version

A version-specific server needs two things: a publish service, given to
`new()`, and a handshake service, given to `build()`. The handshake service
receives the client's `CONNECT` and decides whether to accept it:

```rust,no_run
use ntex::SharedCfg;
use ntex_mqtt::v5;

#[derive(Debug)]
struct ServerError;

impl TryFrom<ServerError> for v5::PublishAck {
    type Error = ServerError;

    fn try_from(err: ServerError) -> Result<Self, Self::Error> {
        Err(err)
    }
}

#[derive(Debug)]
struct Device {
    id: String,
}

async fn handshake(connect: v5::Connect) -> Result<v5::ConnectAck<Device>, ServerError> {
    let packet = connect.packet();
    if packet.username.as_deref() != Some("device") {
        return Ok(connect.failed(v5::codec::ConnectAckReason::BadUserNameOrPassword));
    }
    let device = Device {
        id: packet.client_id.to_string(),
    };
    Ok(connect.ack(device))
}

async fn publish(
    session: &v5::Session<Device>,
    publish: v5::Publish,
) -> Result<v5::PublishAck, ServerError> {
    println!("{} published to {}", session.id, publish.publish_topic());
    Ok(publish.ack())
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    ntex::server::build()
        .bind("mqtt", "127.0.0.1:1883", SharedCfg::default(), async |_| {
            v5::MqttServer::new(publish).build(handshake)
        })?
        .run()
        .await
}
```

The value you pass to `ack()` becomes the connection's session. Every
publish, protocol and control handler can take a `&Session<Device>` as its
first argument and read it, as `publish` does above.
[The handshake](#the-handshake) covers this step in detail.

A v3 server looks the same, with two differences: the publish service
returns `()`, and `ack()` takes a second argument, the "session present"
flag, which tells the client whether you already hold state for it:

```rust,no_run
# use ntex::SharedCfg;
# use ntex_mqtt::v3;
# #[derive(Debug)]
# struct ServerError;
# #[ntex::main]
# async fn main() -> std::io::Result<()> {
ntex::server::build()
    .bind("mqtt", "127.0.0.1:1883", SharedCfg::default(), async |_| {
        v3::MqttServer::new(async |publish: v3::Publish| {
            println!("publish to {}", publish.publish_topic());
            Ok::<_, ServerError>(())
        })
        .build(async |connect: v3::Connect| Ok::<_, ServerError>(connect.ack((), false)))
    })?
    .run()
    .await
# }
```

### The handshake

Every connection starts with a handshake. The server reads the client's first
packet, which must be a `CONNECT` and must arrive within the connect timeout
(see [Configuration](#configuration)). Otherwise the server closes the
connection, and the handshake service is never called.

The handshake service receives the packet wrapped in a `Connect`, which gives
it everything it needs to decide about the client:

- `packet()` is the decoded `CONNECT`: client id, username and password,
  keep-alive, last will and, in MQTT 5, its properties. `packet_mut()` lets
  you take fields out of it instead of copying them.
- `io()` is the connection itself, for querying the peer address or TLS
  details.
- `st()` is the connection state collected before the handshake (see
  [Server state](#server-state)).
- `sink()` is the client's sink, which you can keep for sending to the client
  later.

The service answers with a `ConnectAck`, which is both the `CONNACK` packet
and the outcome of the handshake:

- `ack()` accepts the client and gives the connection its session state.
- `failed()` refuses the client with a reason code. The server sends that
  `CONNACK` and closes the connection. In MQTT 5, `fail_with()` takes a
  complete `CONNACK` packet, so you can add a reason string or other
  properties. MQTT 3.1.1 has fewer codes and a helper for each:
  `identifier_rejected()`, `bad_username_or_pwd()`, `not_authorized()` and
  `service_unavailable()`.
- An error from the handshake service closes the connection without sending
  a `CONNACK` at all.

An accepted `ConnectAck` has a few more settings for the connection. In
MQTT 5, `keep_alive()` sets the Server Keep Alive, and `with()` gives direct
access to the `CONNACK` packet: assigned client id, session expiry, response
information, limits and other properties. In MQTT 3.1.1, `idle_timeout()`
sets how long the connection may stay silent, and `max_packet_size()` limits
incoming packets. Both versions have `max_send()`, which sets the
[send credit](#flow-control-1) for this connection.

#### From acknowledgement to session

`ack()` wraps your value in a `Session<T>`. A session is a cheap,
reference-counted handle that holds three things: your value, which the
session dereferences to; the client's sink, available as `session.sink()`;
and the connection's configuration, available as `session.cfg()`. Since the
session is shared, any part of your value that changes during the connection
needs interior mutability, such as a `Cell` or a `RefCell`.

When the handshake service returns an accepted `ConnectAck`, the server
finishes setting up the connection:

1. It applies the acknowledgement's settings. In MQTT 5, the limits you
   advertise in the `CONNACK`, such as maximum QoS, receive maximum, topic
   alias maximum and maximum packet size, are the limits the server then
   enforces on the client. The send credit never exceeds the receive maximum
   the client announced in its `CONNECT`.
2. It sends the `CONNACK`.
3. It creates the connection's publish, protocol and control services from
   their factories, passing the session to `create()`. These services are
   created once per connection, so a factory you write yourself can prepare
   per-connection resources from the session. If a factory fails, the
   connection is closed.
4. It starts reading packets. The session becomes the state of the
   connection's pipeline, which is why every handler receives it as
   `&Session<T>`.

The session lives as long as something holds a clone of it. Normally that is
the connection's services, so it is dropped when the connection closes.

With `ack()`, only the connection's own services hold the session. If other
parts of your application need to reach the connection, use
`ack_and_session()`, which returns the session
alongside the acknowledgement. Here the handshake registers every device in a
shared map, and the control service removes it again when its connection
stops:

```rust,no_run
use std::{cell::RefCell, collections::HashMap, rc::Rc};

use ntex::SharedCfg;
use ntex_mqtt::{Control, v5};

# #[derive(Debug)]
# struct ServerError;
# impl TryFrom<ServerError> for v5::PublishAck {
#     type Error = ServerError;
#     fn try_from(err: ServerError) -> Result<Self, Self::Error> {
#         Err(err)
#     }
# }
type Registry = Rc<RefCell<HashMap<String, v5::Session<Device>>>>;

struct Device {
    id: String,
    registry: Registry,
}

async fn control(
    session: &v5::Session<Device>,
    msg: Control<ServerError>,
) -> Result<Option<v5::codec::Encoded>, ServerError> {
    if let Control::Stop(_) = msg {
        session.registry.borrow_mut().remove(&session.id);
    }
    Ok(None)
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    ntex::server::build()
        .bind("mqtt", "127.0.0.1:1883", SharedCfg::default(), async |_| {
            let registry = Registry::default();

            v5::MqttServer::new(async |p: v5::Publish| Ok::<_, ServerError>(p.ack()))
                .control(control)
                .build(async move |connect: v5::Connect| {
                    let device = Device {
                        id: connect.packet().client_id.to_string(),
                        registry: registry.clone(),
                    };
                    let (ack, session) = connect.ack_and_session(device);
                    registry.borrow_mut().insert(session.id.clone(), session);
                    Ok::<_, ServerError>(ack)
                })
        })?
        .run()
        .await
}
```

Any handler can now look up another device and publish to it through its
`sink()`. The server calls the factory closure once per worker, so each
worker has its own registry and it never needs locking. Removing the entry
when the connection stops matters: the registry holds a clone of the
session, so without the cleanup the session would never be dropped.

### Serving both versions

Clients state their protocol version in the `CONNECT` packet. The combined
`ntex_mqtt::MqttServer` reads it and hands the connection to a v3 or a v5
server, as the quick start at the top of the chapter shows. Build each
version's server as usual and pass it to `v3()` and `v5()`. A version you do
not configure is refused.

The two servers must use the same error type, but each has its own handlers
and session type. A client has `protocol_version_timeout` from
`MqttServiceConfig` to send enough of its `CONNECT` for the combined server
to read the version.

### Server state

An MQTT server works with three kinds of state, each with its own lifetime.
They all build on the general mechanisms described in the
[state chapter](3-state.md).

**Worker state** lives as long as a server worker. When you start the server
with `build_with_config()`, each worker creates its own state, and the MQTT
server runs in a pipeline that holds it (see
[Worker and request state](6-server-app.md)). The handshake service can read
it if you write the handshake as a
[state-aware function](3-state.md#state-aware-functions) that takes the
worker state as its first argument. Worker state is the place for resources
shared by all connections of a worker, such as a database pool or a device
registry.

**Connection state** is collected before the MQTT protocol starts. A service
in front of the MQTT server can return the connection wrapped in a
[`State`](3-state.md#carrying-state-with-a-request) together with any data it
gathered, for example the peer address or the TLS server name. Create the
server with `MqttServer::with()` instead of `new()` to accept that state; the
handshake then receives a `Connect<T>`, and `connect.st()` returns the
collected value.

**Session state** lives as long as one MQTT connection. It is the value you
pass to `ack()`, and the publish, protocol and control services see it as
`&Session<T>`. The other two kinds of state are not passed to these services
directly, so copy whatever they need into the session during the handshake.
Cloning an `Rc` from the worker state is cheap and gives every handler access
to the worker's shared resources.

The example below uses all three. The worker state holds a list of allowed
devices and a message counter, `accept` records the peer address, and the
handshake combines both into the session:

```rust,no_run
use std::{cell::Cell, collections::HashSet, net::SocketAddr, rc::Rc};

use ntex::io::{Io, types::PeerAddr};
use ntex::service::{State, service};
use ntex::SharedCfg;
use ntex_mqtt::{MqttError, v5};

# #[derive(Debug)]
# struct ServerError;
# impl TryFrom<ServerError> for v5::PublishAck {
#     type Error = ServerError;
#     fn try_from(err: ServerError) -> Result<Self, Self::Error> {
#         Err(err)
#     }
# }
/// Worker state, created once per worker.
#[derive(Clone)]
struct AppState {
    allowed: Rc<HashSet<String>>,
    received: Rc<Cell<u64>>,
}

/// Connection state, collected before the MQTT handshake.
struct Peer {
    addr: Option<SocketAddr>,
}

/// Session state, one per MQTT connection.
struct Device {
    id: String,
    peer: Option<SocketAddr>,
    app: AppState,
}

async fn accept(
    _: &AppState,
    io: Io,
) -> Result<State<Peer, Io>, MqttError<ServerError>> {
    let addr = io.query::<PeerAddr>().get().map(|addr| addr.into_inner());
    Ok(State { req: io, state: Peer { addr } })
}

async fn handshake(
    app: &AppState,
    connect: v5::Connect<Peer>,
) -> Result<v5::ConnectAck<Device>, ServerError> {
    let id = connect.packet().client_id.to_string();
    if !app.allowed.contains(&id) {
        return Ok(connect.failed(v5::codec::ConnectAckReason::NotAuthorized));
    }
    let device = Device {
        id,
        peer: connect.st().addr,
        app: app.clone(),
    };
    Ok(connect.ack(device))
}

async fn publish(
    session: &v5::Session<Device>,
    publish: v5::Publish,
) -> Result<v5::PublishAck, ServerError> {
    let received = &session.app.received;
    received.set(received.get() + 1);
    println!(
        "{} ({:?}) published to {}, {} messages so far",
        session.id,
        session.peer,
        publish.publish_topic(),
        received.get(),
    );
    Ok(publish.ack())
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    ntex::server::build_with_config(async || {
        Ok(AppState {
            allowed: Rc::new(HashSet::from(["sensor-1".to_string()])),
            received: Rc::new(Cell::new(0)),
        })
    })
    .bind("mqtt", "127.0.0.1:1883", SharedCfg::default(), async |_: &AppState| {
        service(accept).and_then(v5::MqttServer::with(publish).build(handshake))
    })?
    .run()
    .await
}
```

Each worker has its own `AppState`, so `received` counts only the messages
handled by that worker. Share a value through an `Arc` if it must be visible
across workers.

### Configuring the server

The server takes its settings from the `SharedCfg` passed to `bind()`:

```rust,no_run
# use ntex::{SharedCfg, time::Seconds};
# use ntex_mqtt::{MqttServiceConfig, QoS, v5};
# #[derive(Debug)] struct ServerError;
# impl TryFrom<ServerError> for v5::PublishAck {
#     type Error = ServerError;
#     fn try_from(err: ServerError) -> Result<Self, Self::Error> { Err(err) }
# }
# #[ntex::main]
# async fn main() -> std::io::Result<()> {
let cfg = SharedCfg::new("MQTT").add(
    MqttServiceConfig::new()
        .set_max_qos(QoS::ExactlyOnce)
        .set_max_size(1024 * 1024)
        .set_max_receive(64)
        .set_connect_timeout(Seconds(10)),
);

ntex::server::build()
    .bind("mqtt", "0.0.0.0:1883", cfg, async |_| {
        v5::MqttServer::new(async |p: v5::Publish| Ok::<_, ServerError>(p.ack()))
            .build(async |c: v5::Connect| Ok::<_, ServerError>(c.ack(())))
    })?
    .run()
    .await
# }
```

An MQTT 5 server announces several of these settings to clients in
`CONNACK`: the maximum QoS, the maximum packet size, the Receive Maximum and
the topic alias limit. MQTT 3.1.1 has no way to announce them, so a client
that exceeds the limits is simply disconnected.

For TLS, bind the server with an ntex TLS acceptor in front of it; the
`openssl` example in the ntex-mqtt repository shows how. To serve MQTT over
WebSockets, upgrade an HTTP/1 connection and hand the WebSocket transport to
the MQTT server, as the [MQTT over WebSocket example] does.

### Handling publishes

The publish service receives a `Publish` for every `PUBLISH` packet. It gives
you the packet's details: `publish_topic()`, `qos()`, `retain()`, `dup()`,
`id()`, and in MQTT 5 the properties through `packet().properties`. If the
client used a topic alias, ntex-mqtt has already resolved it, so the topic is
always complete.

The service's response is the acknowledgement:

- For QoS 0 nothing is sent.
- For QoS 1, ntex-mqtt sends `PUBACK` when the service returns.
- For QoS 2, it sends `PUBREC` when the service returns. The client's `PUBREL`
  then goes to the protocol service, whose default answers with `PUBCOMP`.

Because the ack is sent only after your service returns, returning means "I
have taken responsibility for this message". Store it, forward it, or reject
it before you return.

In MQTT 5, `publish.ack()` creates a successful `PublishAck`. Build one with
`PublishAck::new(reason)` to refuse a message without closing the connection,
and add a reason string or user properties if you like:

```rust
# use ntex_mqtt::v5::{self, codec::PublishAckReason};
# #[derive(Debug)] struct ServerError;
# fn allowed(_: &str) -> bool { true }
async fn publish(publish: v5::Publish) -> Result<v5::PublishAck, ServerError> {
    if !allowed(publish.publish_topic()) {
        return Ok(v5::PublishAck::new(PublishAckReason::NotAuthorized)
            .reason("topic is read-only".into()));
    }
    Ok(publish.ack())
}
```

A service error closes the connection, unless it can become a negative ack.
That is what the `TryFrom<ServerError> for v5::PublishAck` implementation in
the examples decides: returning `Err` keeps the error, returning `Ok(ack)`
sends the ack and keeps the connection open. MQTT 3.1.1 has no negative acks,
so there a service error always closes the connection.

Acks go out in the order publishes arrived, even when the services finish
in a different order. An ack that is ready early waits in the response queue,
whose size `set_max_queue` limits.

### Reading payloads

`publish.read_all().await` returns the whole payload. Small payloads arrive
together with the packet, so this returns at once.

Large payloads are different. When a publish is bigger than what has arrived
so far, ntex-mqtt does not wait for all of it. It calls your service as soon
as the header is in, and streams the rest of the payload to you in chunks of
at least `set_min_chunk_size` bytes. You can still call `read_all()`, or
process the chunks as they come and never hold the whole message in memory:

```rust
# use ntex_mqtt::v5;
# #[derive(Debug)] struct ServerError;
async fn upload(publish: v5::Publish) -> Result<v5::PublishAck, ServerError> {
    let mut total = 0;
    while let Some(chunk) = publish.read().await.map_err(|_| ServerError)? {
        total += chunk.len();
    }
    println!("received {total} of {} bytes", publish.payload_size());
    Ok(publish.ack())
}
```

`payload_size()` tells you the full size up front. If you use `take_payload()`
to move the payload to another task, keep reading it there: dropping a
payload before it has been read to the end closes the connection, because the
rest of the stream has nowhere to go.

### Routing

Rather than one publish service with a big `match` on topics, you can give
`new()` a `Router`. It dispatches publishes to handlers by topic, with a
default handler for anything that does not match:

```rust
# use ntex_mqtt::v5;
# #[derive(Debug)] struct ServerError;
# impl TryFrom<ServerError> for v5::PublishAck {
#     type Error = ServerError;
#     fn try_from(err: ServerError) -> Result<Self, Self::Error> { Err(err) }
# }
# #[derive(Debug)] struct Device;
# fn run() -> std::io::Result<()> {
# let _ = ntex::server::build().bind("mqtt", "127.0.0.1:1883", ntex::SharedCfg::default(), async |_| {
let server = v5::MqttServer::new(
    v5::Router::new(async |publish: v5::Publish| {
        println!("unrouted publish to {}", publish.publish_topic());
        Ok::<_, ServerError>(publish.ack())
    })
    .resource(["alerts", "alarms"], async |publish: v5::Publish| {
        Ok(publish.ack())
    })
    .resource(
        "devices/{id}/telemetry",
        async |session: &v5::Session<Device>, publish: v5::Publish| {
            let id = publish.topic().get("id").unwrap_or_default();
            println!("telemetry from {id} via {session:?}");
            Ok(publish.ack())
        },
    ),
)
.build(async |connect: v5::Connect| Ok::<_, ServerError>(connect.ack(Device)));
# server
# })?;
# Ok(())
# }
```

A resource takes one pattern or a list of them. Patterns match the topic
level by level, `{name}` segments capture a level, and a trailing `#` matches
the rest. Each handler is a full service with its own readiness, so a busy
handler slows down only the topics it serves. `v3::Router` works the same way.

### Sending to clients

A server publishes to a client through that client's sink. Get it from
`connect.sink()` during the handshake, keep it in your session state or a
shared registry, or reach it with `session.sink()` inside a handler:

```rust
# use ntex_mqtt::v5;
# #[derive(Debug)] struct ServerError;
async fn handshake(connect: v5::Connect) -> Result<v5::ConnectAck<()>, ServerError> {
    let sink = connect.sink();
    ntex::rt::spawn(async move {
        let _ = sink
            .publish("welcome")
            .send_at_least_once("hello".into())
            .await;
    });
    Ok(connect.ack(()))
}
```

A server sink has the same publishing API as a client sink: QoS levels,
properties, streaming, ack callbacks, credit and `ready()`. Only
`subscribe()` and `unsubscribe()` are refused, since only clients send those
packets. The sink also closes the connection: `close()` sends `DISCONNECT`,
which in MQTT 5 can carry a reason code via `close_with_reason()`.

There is one trap to avoid. Acks from the client are read by the same
connection that delivers its publishes, and the connection stops reading
while it is busy (see the next section). A publish handler that awaits the
ack of a message it sent back to *the same client* can therefore wait
forever: the ack cannot be read until the handler returns. Spawn such sends,
as the handshake above does, or use `send_at_least_once_no_block()` with an
ack callback.

### Flow control

ntex-mqtt protects the server from fast or misbehaving clients in a few
layers.

**Incoming publishes.** `set_max_receive` limits how many QoS 1 and 2
publishes a connection handles at once. An MQTT 5 server announces it as
Receive Maximum, and a client that sends more is breaking the protocol. MQTT
3.1.1 has no such announcement, so extra publishes wait in order for a free
slot.

**Read pause.** The connection stops reading new packets while:

- the publish or protocol service is not ready;
- the response queue is full (`set_max_queue`);
- the publishes being handled add up to `set_max_receive_size` bytes;
- a streaming payload has `set_max_payload_buffer_size` bytes your handler
  has not read yet. Reading resumes once the handler has read the buffer
  down to half that size.

While reading is paused, the client's data waits in its TCP buffers, so TCP
flow control eventually slows the client down. No read timeout runs during a
pause, because the delay is the server's own doing, not the client's.

**Outgoing publishes.** Publishes to a client use send credit, just as on the
client side. The credit is `set_max_send`, or `max_send()` on the
`ConnectAck`, and in MQTT 5 at most the client's Receive Maximum. Sends wait
for credit and for write backpressure, so a slow client slows its senders
down instead of filling server memory.

Your own services take part through readiness. If a publish handler reports
that it is not ready, for example because a downstream queue is full, the
connection stops reading until it is. Services whose readiness can change
must notify ntex-mqtt through `ReadyNotifier` in `bind()`; the
[pipeline chapter](2-pipeline.md) explains how.

### The protocol service

The protocol service handles everything a client sends apart from publishes.
Without one, the server answers pings, `DISCONNECT` and `PUBREL`, and closes
the connection on anything else, including subscriptions. A broker therefore
always needs one.

It receives a `ProtocolMessage` and returns a `ProtocolMessageAck`:

```rust
# use ntex::util::ByteString;
# use ntex_mqtt::v5::{self, ProtocolMessage, ProtocolMessageAck, QoS};
# use std::cell::RefCell;
# #[derive(Debug)] struct ServerError;
# impl TryFrom<ServerError> for v5::PublishAck {
#     type Error = ServerError;
#     fn try_from(err: ServerError) -> Result<Self, Self::Error> { Err(err) }
# }
#[derive(Default)]
struct Client {
    topics: RefCell<Vec<ByteString>>,
}

async fn protocol(
    session: &v5::Session<Client>,
    msg: ProtocolMessage,
) -> Result<ProtocolMessageAck, ServerError> {
    Ok(match msg {
        ProtocolMessage::Subscribe(mut subscribe) => {
            for mut sub in subscribe.iter_mut() {
                if sub.topic().starts_with("$SYS") {
                    sub.fail(v5::codec::SubscribeAckReason::NotAuthorized);
                } else {
                    session.topics.borrow_mut().push(sub.topic().clone());
                    sub.confirm(QoS::AtLeastOnce);
                }
            }
            subscribe.ack()
        }
        ProtocolMessage::Unsubscribe(unsubscribe) => {
            let mut topics = session.topics.borrow_mut();
            topics.retain(|t| unsubscribe.iter().all(|u| u != t));
            unsubscribe.ack()
        }
        msg => msg.ack(),
    })
}
# fn run() -> std::io::Result<()> {
# let _ = ntex::server::build().bind("mqtt", "127.0.0.1:1883", ntex::SharedCfg::default(), async |_| {
let server = v5::MqttServer::new(async |p: v5::Publish| Ok::<_, ServerError>(p.ack()))
    .protocol(protocol)
    .build(async |c: v5::Connect| Ok::<_, ServerError>(c.ack(Client::default())));
# server
# })?;
# Ok(())
# }
```

For a `SUBSCRIBE`, visit each topic filter and either `confirm()` it with the
QoS you grant or `fail()` it. Filters you leave alone are refused. In MQTT 5
each one has `options()` with the client's QoS, no-local, retain-as-published
and retain-handling choices, and the whole packet is available through
`packet()`. Unsubscribe works the same way, with `success()` and `fail()`.

The other messages are:

- `Ping`, answered with `PINGRESP` by `ack()`;
- `Disconnect`, the client's `DISCONNECT`. In MQTT 5 it carries a reason code
  and may update the session expiry interval;
- `PublishRelease`, the `PUBREL` that finishes a QoS 2 publish;
- `Auth`, MQTT 5 only, a step of an enhanced authentication exchange. Answer
  with `auth.ack(packet)`.

Any message can be turned into a disconnect with `disconnect()` (in MQTT 5
`disconnect_with()` picks the reason code).

Protocol messages are handled one at a time, in order, with up to 16
buffered.

### The control service

The control service is where you observe connections coming and going. It
receives the same `Control` messages as on the client, and can return a
packet to send before the connection closes:

```rust
# use ntex_mqtt::{Control, Reason, v5};
# #[derive(Debug)] struct ServerError;
# impl TryFrom<ServerError> for v5::PublishAck {
#     type Error = ServerError;
#     fn try_from(err: ServerError) -> Result<Self, Self::Error> { Err(err) }
# }
# #[derive(Debug)] struct Device;
async fn control(
    session: &v5::Session<Device>,
    msg: Control<ServerError>,
) -> Result<Option<v5::codec::Encoded>, ServerError> {
    match msg {
        Control::Stop(Reason::Error(err)) => {
            println!("{session:?}: service error {:?}", err.get_ref());
            let disconnect = v5::codec::Disconnect::new(
                v5::codec::DisconnectReasonCode::ImplementationSpecificError,
            );
            Ok(Some(v5::codec::Packet::from(disconnect).into()))
        }
        Control::Stop(Reason::Protocol(err)) => {
            println!("{session:?}: protocol error {:?}", err.get_ref());
            Ok(None)
        }
        Control::Stop(Reason::PeerGone(_)) => {
            println!("{session:?}: connection lost");
            Ok(None)
        }
        Control::WrBackpressure(bp) => {
            println!("{session:?}: write backpressure {}", bp.enabled());
            Ok(None)
        }
    }
}
# fn run() -> std::io::Result<()> {
# let _ = ntex::server::build().bind("mqtt", "127.0.0.1:1883", ntex::SharedCfg::default(), async |_| {
let server = v5::MqttServer::new(async |p: v5::Publish| Ok::<_, ServerError>(p.ack()))
    .control(control)
    .build(async |c: v5::Connect| Ok::<_, ServerError>(c.ack(Device)));
# server
# })?;
# Ok(())
# }
```

`Stop` arrives exactly once per accepted connection, which makes it the place
to clean up: remove the client from a registry, publish its presence, release
resources. A graceful `DISCONNECT` from the client goes to the protocol
service first, then the connection stops.

When MQTT 5 shuts a connection down because of a protocol error, ntex-mqtt
sends the matching `DISCONNECT` reason code itself, so returning `None` is
usually right. For MQTT 3.1.1, which has no server `DISCONNECT`, return
`None`.

### MQTT 5 features

Most MQTT 5 additions are packet fields and properties, and ntex-mqtt gives
you direct access to them rather than hiding them behind policy:

- **Reason codes** appear in `CONNACK`, `PUBACK`, `SUBACK`, `UNSUBACK` and
  `DISCONNECT`, through the builders shown above.
- **User properties and reason strings** can be added to acks with
  `properties()` and `reason()`, and read from any packet.
- **Topic aliases** from clients are resolved before your publish service
  sees the message, up to `set_max_topic_alias` aliases per connection. To
  send aliases to a client, set `properties.topic_alias` on your publishes,
  within the limit the client gave in `CONNECT`.
- **Session expiry** and **will delay** come from the `CONNECT` and
  `DISCONNECT` packets; storing and expiring sessions is up to your
  application.
- **Request/response** uses the response topic and correlation data
  properties of a publish.
- **Subscription identifiers** are in the `SUBSCRIBE` packet; attach them to
  matching publishes with `properties.subscription_ids`.
- **Server Keep Alive** and **assigned client identifiers** are set on the
  `ConnectAck`.
- **Enhanced authentication** runs through `Auth` protocol messages.

ntex-mqtt is a protocol framework rather than a ready-made broker. It handles
the protocol rules, ordering, flow control and limits; matching subscriptions
to publishes, retained messages and persistent sessions are left to you, so
you can store them however your application needs.

## Summary

- ntex-mqtt provides an MQTT client and server, each for MQTT 3.1.1 (`v3`)
  and MQTT 5 (`v5`). The combined `MqttServer` accepts both on one port.
- Each connection has a publish service, a protocol service for other packets
  and a control service for lifecycle events. A sink sends packets to the
  peer, and a session holds your per-connection state.
- `MqttServiceConfig` in a `SharedCfg` sets limits, flow control and
  timeouts.
- The client connects through `MqttConnector`, publishes and subscribes
  through its sink, and handles incoming messages with resources and a
  protocol service. Its dispatcher must be running for acks to arrive.
- The server accepts or rejects clients in the handshake service, handles
  publishes directly or through a `Router`, answers subscriptions in the
  protocol service, and sends to clients through their sinks.
- Acks are sent when your service returns, in arrival order. Send credit,
  write backpressure and read pause keep both sides from overrunning each
  other. Never await an ack from the client whose publish you are handling.

For a complete program, see the [MQTT over WebSocket example]: an MQTT 3.1.1
server behind an HTTP/1 WebSocket upgrade, plus a client that publishes to
it. The [ntex-mqtt examples] cover more ground, including TLS, sessions,
routing and subscriptions.

[MQTT]: https://mqtt.org
[ntex-mqtt]: https://docs.rs/ntex-mqtt
[ntex-mqtt examples]: https://github.com/ntex-rs/ntex-mqtt/tree/main/examples
[MQTT over WebSocket example]: https://github.com/ntex-rs/examples/tree/main/protocols/mqtt
[`SharedCfg`]: https://docs.rs/ntex/latest/ntex/struct.SharedCfg.html
[`IoConfig`]: https://docs.rs/ntex/latest/ntex/io/struct.IoConfig.html
[`Pipeline`]: https://docs.rs/ntex/latest/ntex/struct.Pipeline.html
