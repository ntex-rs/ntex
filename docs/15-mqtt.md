# MQTT with ntex-mqtt

[MQTT] is a lightweight publish/subscribe protocol used in IoT, telemetry
and messaging systems. Small devices often use it to keep long-lived
connections to a broker. [ntex-mqtt] provides an MQTT client and server for
ntex. Both support the two protocol versions in use, MQTT 3.1.1 and
MQTT 5.0. A single server can accept both versions on the same port.

ntex-mqtt uses the same building blocks as the rest of ntex. Connections are
ntex I/O objects, and handlers are ntex services. You pass settings through a
[`SharedCfg`] and run MQTT servers inside the [ntex server](5-server.md).
These pieces will be familiar if you have written an ntex web service.

Add the crate alongside ntex:

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
Its `TryFrom` implementation decides how to handle an error. It can turn
the error into a `PUBACK` that refuses the message, or close the connection.
Returning `Err` closes it. [Handling publishes](#handling-publishes) shows
how to return a negative ack instead.

## How ntex-mqtt is organized

Choose `ntex_mqtt::v3` for MQTT 3.1.1 or `ntex_mqtt::v5` for MQTT 5.
The modules have the same structure, so code for one version translates to
the other almost line by line. MQTT 5 adds reason codes, properties and a
few new packets, so its types carry more information.

Within either module, start with `MqttServer` to accept connections or
`client::MqttConnector` to open one. Incoming messages arrive as `Publish`
values; a `Router` can dispatch them by topic. Use `MqttSink` to send
packets back, and look in `codec` when you need the packet types themselves.
The crate root holds the combined `MqttServer`, the shared
`MqttServiceConfig` settings, and the `Control` messages for connection
events.

Once connected, your application splits its work between up to three
services. The publish service handles incoming `PUBLISH` packets. On a
server, this is where most application logic lives. On a client, router
handlers and the protocol service fill that role.

The protocol service handles packets that need an answer but are not
publishes. On the server these are `SUBSCRIBE`, `UNSUBSCRIBE`, `PINGREQ`,
`DISCONNECT`, `PUBREL` and, in MQTT 5, `AUTH`. It returns the response to
send back. The client has a corresponding service for packets from the
server, described [below](#the-protocol-service).

The control service handles connection events rather than MQTT packets.
It reports write backpressure and tells you when the connection stops,
including the reason. Both the protocol and control services have default
implementations; replace them when your application needs different
behavior.

Use a **sink** (`MqttSink`) to send packets to the peer, including publishes,
subscriptions and the final `DISCONNECT`. It is a cheap, cloneable handle.

A **session** (`v3::Session<St>` or `v5::Session<St>`) bundles the sink with
your per-connection state `St`. It dereferences to `St`, and its `sink()`
method returns the sink.

## Configuration

A device sending telemetry and a client uploading files may need different
packet limits. Put those limits and protocol timeouts in
`MqttServiceConfig`, then add it to a [`SharedCfg`] as you would with other
ntex configuration. Connections created with that `SharedCfg` use the
settings that apply to their protocol version and role:

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

An [`IoConfig`] in the same `SharedCfg` controls the connection's buffers,
write backpressure and I/O timeouts. MQTT connections use ordinary ntex I/O
objects; see the [I/O chapter](7-io.md).

The first group of protocol settings limits what a peer may send you:

| Setting | Default | Meaning |
|---|---|---|
| `set_max_qos` | `AtLeastOnce` | Highest QoS a peer may publish with. A higher QoS is a protocol violation and closes the connection. An MQTT 5 server announces it in `CONNACK` |
| `set_max_size` | 256 KiB | Largest incoming packet for MQTT 3.1.1 connections and MQTT 5 servers; 0 means the protocol maximum of 256 MiB. An MQTT 5 server announces it in `CONNACK` |
| `set_max_topic_alias` | 32 | Topic aliases an MQTT 5 client may use when publishing to the server |
| `set_check_subs_availability` | `true` | MQTT 5 server only. Close the connection when a client subscribes to a shared or wildcard filter that `CONNACK` declared unavailable. When disabled, the protocol service decides |

The next group controls how much work a connection handles at once. See
[Flow control](#flow-control) for details:

| Setting | Default | Meaning |
|---|---|---|
| `set_max_receive` | 16 | Concurrent incoming publishes in MQTT 3.1.1; extra publishes wait, and 0 disables the limit. An MQTT 5 server advertises this as Receive Maximum for QoS 1 and 2; 0 means 65535 |
| `set_max_receive_size` | 65,535 bytes | Total size of incoming packets being handled. When it is reached, the connection stops reading |
| `set_max_queue` | 64 | Responses waiting to be sent. Acks go out in the order the publishes arrived; when the queue is full, the connection stops reading |
| `set_max_send` | 16 | Outgoing QoS 1 and 2 publishes waiting for an ack on servers and MQTT 3.1.1 clients. An MQTT 5 server also applies the client's Receive Maximum |
| `set_min_chunk_size` | 32 KiB | How much of a large payload to collect before handing a chunk to your handler |
| `set_max_payload_buffer_size` | 32 KiB | Unread payload to buffer for a streaming publish before the connection stops reading |
| `set_handle_qos_after_disconnect` | `None` | Highest QoS of received publishes that are still handed to the publish service after the connection starts closing. By default they are dropped |

MQTT 5 clients announce their own incoming packet-size and receive limits
through `Connect::max_packet_size()` and `Connect::max_receive()`, rather
than the corresponding `MqttServiceConfig` settings. Their outgoing send
credit comes from the server's `CONNACK`, not `set_max_send`.

The remaining settings control timeouts:

| Setting | Default | Meaning |
|---|---|---|
| `protocol_version_timeout` | 5 s | Combined server only. Time a client has to send enough of `CONNECT` to reveal its protocol version |
| `set_connect_timeout` | 5 s | Server only. Time a client has to send its `CONNECT` packet |
| `set_handshake_timeout` | disabled | Client only. Time allowed for connecting, sending `CONNECT` and receiving `CONNACK` |

A client chooses its keep-alive in the `CONNECT` packet. The server may
override it while accepting the connection. This setting is separate from
`MqttServiceConfig`.

## The client

Use the client service to open a connection and perform the MQTT handshake.
It returns a `Client` when the handshake succeeds. The examples here use
MQTT 5. The MQTT 3.1.1 client in `v3::client` works the same way, with the
differences described where they apply.

### Connecting

To connect, wrap `v5::client::MqttConnector` in a [`Pipeline`] with your
`SharedCfg`. The connector is an ntex service. Call it with a
`v5::client::Connect` describing the connection:

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

`Connect` builds the `CONNECT` packet. The `client_id` identifies the
session the server should look up. If you want to discard any stored
session instead, set `clean_start`; MQTT 3.1.1 calls this `clean_session`.
For authentication, set `username` and `password`, or use MQTT 5's `auth`
for enhanced authentication.

Use `keep_alive` to choose how often the client pings an idle connection;
it is disabled by default. A `last_will` gives the server a message to
publish if the client disappears without disconnecting.

MQTT 5 also lets you announce the client's limits with `max_packet_size`
and `max_receive`. To set user properties, use `properties`; for direct
access to the remaining packet fields, use `packet`.

The connector returns a `Client` once the server accepts the connection. If
the server rejects it, the error carries the server's `CONNACK`, including
its reason code. Use `client.packet()` to read the full `CONNACK` after a
successful handshake.

A successful MQTT 5 `CONNACK` also tells the client how much it may send.
Receive Maximum limits unacknowledged QoS 1 and 2 publishes, and Maximum
Packet Size limits the size of outgoing packets. If the server supplies a
Server Keep Alive, it replaces the client's own keep-alive interval.

### Secure connections

To connect over TLS, pass a transport connector to `connector()`. Without
one, the connector opens a plain TCP connection. You can use any ntex
connector, such as openssl:

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

The standard ports are `ntex_mqtt::TLS_PORT` (8883) and `TCP_PORT` (1883).
You can use the same transport mechanism for MQTT over WebSockets. The
`mqtt-ws-client` example in the ntex-mqtt repository shows how. The
[MQTT over WebSocket example] includes a complete client and server.

### Running the connection

Start the client's dispatcher to read packets, answer pings and deliver
acks to waiting publishes. A `Client` does none of this on its own. Each
start method consumes the `Client`:

- `start_default()` runs the connection with default handlers. Any publish
  from the server closes the connection, so this suits a client that only
  sends.
- `start(service)` runs it with your protocol service. That service sees
  every publish and every other packet the server sends.
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

The client sends keep-alive pings while the dispatcher runs. The dispatcher
also reads acks. If you never start it, a publish waiting for an ack waits
forever.

### Publishing

To send a message, call `sink.publish(topic)` to get a `PublishBuilder`.
Set the packet options you need, then send the payload with your chosen QoS:

| Method | QoS | Completes when | Returns |
|---|---|---|---|
| `send_at_most_once` | 0 | The packet is written | `()` |
| `send_at_least_once` | 1 | The server sends `PUBACK` | `PublishAck` (`()` in MQTT 3.1.1) |
| `send_exactly_once` | 2 | The server sends `PUBREC` | `PublishReceived` |

A QoS 1 publish completes with the server's `PUBACK`. In MQTT 5, the ack's
reason code can refuse a message without closing the connection. For
example, the server might return `NotAuthorized` or `QuotaExceeded`. Check
`reason_code` when it matters.

An error from the call itself means the publish could not be sent or
acknowledged. This usually happens because the connection closed.

QoS 2 takes two round trips. `send_exactly_once` returns once the server has
received the message. Call `release()` on the result to send `PUBREL` and
wait for `PUBCOMP`. If you drop the `PublishReceived` instead, ntex-mqtt
releases the publish for you.

Use `properties()` to add MQTT 5 properties to a publish. These include user
properties, content type and message expiry. You can also set a response
topic and correlation data for request/response messages, among other
properties:

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

ntex-mqtt assigns packet ids automatically. You can set them yourself with
`packet_id()`. Do not mix automatic and manual ids on one connection, or
they will collide.

### Publishing without waiting

Awaiting each ack limits a task to one message per round trip. To keep
several publishes in flight from one task, register an ack callback. Then
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

For these non-blocking sends, the callback reports acks in arrival order.
When the connection closes, it also reports each non-blocking publish
still in flight, with `disconnected` set to `true`.

The callback runs while the sink is borrowed, so it must not call any sink
method. Hand that work to a channel or a task instead.

`send_at_least_once_no_block()` panics if the sink is not ready. Always
check `is_ready()` first, or wait for `ready()` as described below.

### Streaming large payloads

A publish normally carries its whole payload in one `Bytes`. For large
messages, you can send it in pieces instead. Give the builder the total
size to get a `StreamingPayload`, then send each piece through it:

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

The example joins the ack future and the payload sender so both can make
progress. `payload.send()` waits until the publish header has been written.
The ack only arrives after the last chunk.

The total size of all chunks must match the declared size exactly. Use
`stream_at_most_once(size)` to stream a QoS 0 publish.

Other publishes on the same connection wait until the streaming publish
finishes. MQTT cannot interleave packets inside a payload.

Incoming publishes can be streamed as well; see
[Reading payloads](#reading-payloads).

### Flow control

Send credit limits how many QoS 1 and 2 publishes can wait for an ack at
once. The server tells the client this limit through its Receive Maximum,
and ntex-mqtt tracks the available **send credit**.

Each QoS 1 or 2 publish takes one unit of credit when sent. The credit
returns when the ack arrives. When none is left, `send_at_least_once()` and
`send_exactly_once()` wait for an ack to free some. Waiting publishes get
credit in the order they requested it; QoS 0 publishes do not use credit.

The sink also follows the connection's write backpressure. Sends wait for
the write buffer to drain when it grows past its high watermark. See the
[I/O chapter](7-io.md) for details.

To check before sending, use `credit()` for the remaining credit or
`is_ready()` for both credit and the absence of write backpressure. When
you need to wait, `ready().await` returns once the sink is ready, or
returns `false` if the connection has closed.

This lets a producer slow down rather than queue up. For example, wait for
readiness before sending each sensor reading:

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

`ready()` does not reserve credit. If several tasks share a sink, another
task may use the credit between your `ready()` call and your send. An
awaiting send waits again if needed. The non-blocking send would panic, so
check `is_ready()` immediately before calling
`send_at_least_once_no_block()`.

For incoming publishes, the client announces its Receive Maximum with
`Connect::max_receive()`. The settings in [Configuration](#configuration)
also limit how much incoming data it buffers.

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

The `SUBACK` holds one status per filter, in filter order. Each status gives
the granted QoS or a reason the server refused that filter.
`sink.unsubscribe()` works the same way.

In MQTT 3.1.1, `subscribe()` takes no identifier. Its `topic_filter()` takes
just a QoS, and the ack is a list of `SubscribeReturnCode`s.

Subscribing does not use send credit. The dispatcher must still be running
to read the server's answer.

### Receiving publishes

Attach handlers to topic patterns with `resource()` to receive publishes
from your subscriptions. This turns the `Client` into a `ClientRouter`.
Start the router as you would a client:

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

Resource patterns match topic levels. A `{name}` segment matches and
captures one level; read it with `publish.topic().get("name")`. A trailing
`#` matches the rest of the topic.

A handler is any service that takes a `Publish` and returns a `PublishAck`.
It can also take `&v5::Session<()>` as its first argument. MQTT 3.1.1
handlers return `()`.

The router uses the handler's response to acknowledge each publish. It
sends `PUBACK` for QoS 1 and `PUBREC` for QoS 2. In MQTT 5, a handler can
refuse a message with `PublishAck::new(reason)`.

A handler error closes the connection unless it converts to a `PublishAck`.
The `TryFrom` implementation above controls this. Returning `Ok(ack)` from
it turns the error into a negative ack.

With `start_default()`, a publish whose topic matches no resource closes the
connection. To handle those, and every other packet from the server, use
`start()` with a protocol service.

### The protocol service

Routing covers the topics you expect, but a client also needs a policy for
anything else the server sends. A client protocol service receives these
packets as `v5::client::ProtocolMessage` values. An unrouted publish arrives
as `Publish`; `PublishRelease` carries the server's `PUBREL` for a QoS 2
publish. The service also sees `Ping` for a `PINGRESP` and, in MQTT 5,
`Disconnect` for a server `DISCONNECT`.

For an unrouted `Publish`, MQTT 5's `ProtocolMessage::ack()` closes the
connection because publishing is unsupported by default. Match that variant
and use `publish.ack(reason)` to accept or reject it. In MQTT 3.1.1,
`ProtocolMessage::ack()` and `publish.ack()` both acknowledge the publish.
Other variants' `ack()` methods provide their default responses.

To close instead, MQTT 5's `ProtocolMessage::disconnect()` takes a
`codec::Disconnect` packet. MQTT 3.1.1 has no public equivalent on
`ProtocolMessage`; use the sink's `close()`.

This MQTT 5 service reads and accepts unrouted publishes:

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

Use the control service for events outside the MQTT packet exchange, such
as logging a lost connection or observing a slow peer. It receives
`ntex_mqtt::Control` values: `WrBackpressure` reports when write
backpressure starts and stops, while `Stop(reason)` arrives once as the
connection shuts down.

The stop reason tells you where the failure came from. `Error` means one
of your services returned an error. `Protocol` means the peer broke the
protocol or a keep-alive timed out; `PeerGone` means the underlying
connection closed.

The service returns an optional packet to send before closing, usually a
`DISCONNECT` with a reason code. The default control service returns no
packet. Set your client control service with `start_with_control()`; the
[server section](#the-control-service-1) shows a full example.

### Closing

Call `sink.close()` to send `DISCONNECT`, flush buffered data and close the
connection gracefully. In MQTT 5, use `close_with_reason()` to choose the
`DISCONNECT` packet. For example, you can include a session expiry interval
or a reason code.

To drop the connection immediately, use `force_close()`. This discards
buffered data and fails any publishes still in flight.

`sink.is_open()` becomes `false` as soon as either side starts closing the
connection.

## The server

An MQTT server is a service factory that you plug into the
[ntex server](5-server.md). Use `v3::MqttServer` to serve MQTT 3.1.1 or
`v5::MqttServer` to serve MQTT 5. To accept both versions, combine them with
`ntex_mqtt::MqttServer`.

### A server for one version

To build a server for one protocol version, pass a publish service to
`new()`. Then pass a handshake service to `build()`. The handshake service
receives the client's `CONNECT` and decides whether to accept the
connection:

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

The value you pass to `ack()` becomes the connection's session. Publish,
protocol and control handlers can read it by taking `&Session<Device>` as
their first argument, as `publish` does above.
[The handshake](#the-handshake) covers this step in detail.

A v3 server has the same structure. Its publish service returns `()`.
Its `ack()` also takes a second argument, the "session present" flag. This
tells the client whether you already hold state for it:

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

The handshake is where you turn an incoming connection into an accepted
client with application state. For example, you can check credentials,
look up the device, and put its identity into the session before allowing
it to publish.

The service receives the client's first packet wrapped in a `Connect`.
Read `packet()` for the decoded `CONNECT`: the client id, username and
password, keep-alive, last will and, in MQTT 5, properties. Use
`packet_mut()` if you need to take fields out rather than copy them.

The wrapper also gives you context beyond the packet. Use `io()` to query
the connection's peer address or TLS details, and `st()` for data collected
before the handshake; see [Server state](#server-state). Keep a handle from
`sink()` if another part of your application will send to this client.

To accept the client, return `ack()` with its session state. To refuse it,
return `failed()` with a reason code. Both produce a `ConnectAck`, which
holds the `CONNACK` packet; after a refusal, the server sends it and closes
the connection. Returning an error instead closes the connection without a
`CONNACK`, so the client does not get a refusal reason.

For an MQTT 5 refusal, `fail_with()` takes a complete `CONNACK` packet.
Use it to include a reason string or other properties. MQTT 3.1.1 has fewer
codes and a helper for each: `identifier_rejected()`, `bad_username_or_pwd()`,
`not_authorized()` and `service_unavailable()`.

Acceptance is also your chance to adjust settings for this client. In
MQTT 5, `keep_alive()` adjusts the idle timeout and advertises Server Keep
Alive when needed. Use `with()` to edit the `CONNACK` packet directly,
including an assigned client id, session expiry, response information,
limits and other properties.

In MQTT 3.1.1, use `idle_timeout()` for how long the connection may stay
silent and `max_packet_size()` to limit incoming packets. Both versions
provide `max_send()` to set the connection's [send credit](#flow-control-1).

Before calling your service, the server checks the first packet. It must be
`CONNECT` and arrive within the connect timeout; see
[Configuration](#configuration). Otherwise, the server closes the connection
without calling the handshake service.

#### From acknowledgement to session

The value you pass to `ack()` becomes the state your handlers see through
`Session<T>`. This cheap, reference-counted handle dereferences to your
value and also provides the client's sink through `session.sink()` and
the connection's configuration through `session.cfg()`.

Because handlers share the session, changing your value during the
connection requires interior mutability, such as a `Cell` or a `RefCell`.

When the handshake service returns an accepted `ConnectAck`, the server
finishes setting up the connection:

1. It applies the acknowledgement's settings. In MQTT 5, the server enforces
   the limits you advertise in `CONNACK`. These include maximum QoS,
   receive maximum, topic alias maximum and maximum packet size. Send credit
   never exceeds the receive maximum the client announced in `CONNECT`.
2. It sends the `CONNACK`.
3. It creates the publish, protocol and control services from their
   factories, passing the session to `create()`. Each factory runs once per
   connection and can use the session to prepare per-connection resources.
   If a factory fails, the server closes the connection.
4. It starts reading packets. The session becomes the connection pipeline's
   state, so every handler receives it as `&Session<T>`.

The session lives as long as something holds a clone of it. Normally, the
connection's services are its only owners, so it is dropped when they
finish. To keep a handle elsewhere in your application, use
`ack_and_session()`, which returns the session alongside the
acknowledgement.

For example, the handshake can register each device in a shared map so
other handlers can reach it. The control service removes the entry when
the connection stops:

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
`sink()`. The server calls the factory closure once per worker. Each worker
therefore has its own registry, with no need for locking.

Remove the registry entry when the connection stops. The entry holds a
clone of the session; without this cleanup, the session would never be
dropped.

### Serving both versions

To accept both protocol versions on one port, use the combined
`ntex_mqtt::MqttServer`, as in the opening example. Build each version's
server as usual. Pass the MQTT 3.1.1 server to `v3()` and the MQTT 5 server
to `v5()`.

The combined server reads the version from the client's `CONNECT` and
passes the connection to the matching server. It refuses versions you have
not configured. Both servers must use the same error type, though each has
its own handlers and session type.

The client must send enough of `CONNECT` to reveal its version within
`protocol_version_timeout`, set in `MqttServiceConfig`.

### Server state

Choose where to keep state based on how long you need it and which
connections need access. An MQTT server uses worker, connection and session
state. All three use the mechanisms described in the
[state chapter](3-state.md).

A database pool or device registry usually belongs in worker state, shared
by all connections on that worker. With `build_with_config()`, each worker
creates its own state, held by the pipeline that runs the MQTT server;
see [Worker and request state](6-server-app.md). To read it in the
handshake, use a [state-aware function](3-state.md#state-aware-functions)
with worker state as its first argument.

Data collected for one connection before MQTT starts, such as the peer
address or TLS server name, belongs in connection state. A service in
front of the MQTT server can wrap that data and the connection in a
[`State`](3-state.md#carrying-state-with-a-request). Create the MQTT server
with `MqttServer::with()` rather than `new()` to accept it. Your handshake
then receives a `Connect<T>` and reads the value with `connect.st()`.

The handshake brings these sources together in session state: the value
you pass to `ack()`. Publish, protocol and control services receive that
value through `&Session<T>`, not the original worker or connection state.
Copy or clone what those handlers need into the session. For shared worker
resources, cloning an `Rc` gives every handler access without copying the
resource itself. A session belongs to one MQTT connection, though retained
clones can keep its state alive after the connection closes.

The example below uses all three kinds of state. Worker state holds a list
of allowed devices and a message counter. The `accept` service records the
peer address, and the handshake combines both into the session:

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

Each worker has its own `AppState`. Its `received` counter only counts
messages handled by that worker. Share a value through an `Arc` if it must
be visible across workers.

### Configuring the server

Pass your `SharedCfg` to `bind()` to apply the settings described in
[Configuration](#configuration).

An MQTT 5 server announces its maximum QoS, maximum packet size, Receive
Maximum and topic alias limit in `CONNACK`. MQTT 3.1.1 cannot announce these
limits. A client that exceeds them is disconnected.

For TLS, put an ntex TLS acceptor in front of the server when binding it.
The `openssl` example in the ntex-mqtt repository shows how. For MQTT over
WebSockets, upgrade an HTTP/1 connection and pass the WebSocket transport to
the MQTT server. See the [MQTT over WebSocket example].

### Handling publishes

Use the publish service to process messages from clients. It receives a
`Publish` for each `PUBLISH` packet and returns the acknowledgement.

Read the topic with `publish_topic()`. Use `qos()` to read the QoS level.
The `retain()`, `dup()` and `id()` methods expose the other packet details.
In MQTT 5, use `packet().properties` to read properties.

ntex-mqtt resolves topic aliases before calling your service, so the topic
is always complete.

Returning from the service means you have taken responsibility for the
message, so store, forward or reject it before returning. For QoS 1,
ntex-mqtt then sends `PUBACK`; for QoS 2, it sends `PUBREC`. The client's
later `PUBREL` goes to the protocol service, whose default answers with
`PUBCOMP` to complete the QoS 2 exchange. QoS 0 sends no acknowledgement.

In MQTT 5, `publish.ack()` creates a successful `PublishAck`. Use
`PublishAck::new(reason)` to refuse a message without closing the
connection. You can also add a reason string or user properties:

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

A service error closes the connection unless it converts to a negative
ack. The `TryFrom<ServerError> for v5::PublishAck` implementation in the
examples makes that decision. Returning `Err` keeps the error. Returning
`Ok(ack)` sends the ack and keeps the connection open.

MQTT 3.1.1 has no negative acks. A service error therefore always closes the
connection.

Acks go out in the order publishes arrived, even if the services finish in
a different order. An ack that is ready early waits in the response queue.
The `set_max_queue` setting limits that queue's size.

### Reading payloads

Call `publish.read_all().await` to read the whole payload. For small
payloads, this returns at once because the payload arrives with the packet.
To avoid holding a large message in memory, read it one chunk at a time.

If only part of a publish has arrived, ntex-mqtt calls your service as soon
as it has the header. It streams the remaining payload in chunks of at
least `set_min_chunk_size` bytes. You can still use `read_all()`, or process
each chunk as it arrives:

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

Use `payload_size()` to get the full size before reading. You can move the
payload to another task with `take_payload()`, but that task must keep
reading it. Dropping a payload before reading it to the end closes the
connection, because the rest of the stream has nowhere to go.

### Routing

Use a `Router` to send publishes to handlers by topic. Pass it to `new()`
as the publish service, with a default handler for topics that do not
match. This avoids putting all your topic handling in one large `match`:

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

A resource takes one pattern or a list of patterns, using the syntax
[described for the client](#receiving-publishes). Each handler is a full
service with its own readiness, but the router waits for all handlers and
the default service to be ready. A handler that is not ready can therefore
pause the whole connection, not just its own topics. `v3::Router` works the
same way.

### Sending to clients

To send a message to a client, publish through that client's sink. Get it
with `connect.sink()` during the handshake and keep it in session state or
a shared registry. Inside a handler, you can use `session.sink()` instead:

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

A server sink has the same publishing API as a client sink. It supports the
same QoS levels, properties, streaming and ack callbacks. Send credit and
`ready()` work the same way too. It refuses `subscribe()` and
`unsubscribe()`, since only clients send those packets.

Call `close()` on the sink to close the connection gracefully. An MQTT 5
server sends `DISCONNECT` first; use `close_with_reason()` to include a
reason code. An MQTT 3.1.1 server closes without a `DISCONNECT`, since that
version only allows clients to send it.

Do not await an ack inside a publish handler when sending to *the same
client*. The connection reads both the client's publishes and its acks. It
stops reading while busy, as described in the next section. The handler
can therefore wait forever for an ack that cannot be read until it returns.

Spawn these sends, as the handshake above does. You can also use
`send_at_least_once_no_block()` with an ack callback.

### Flow control

Flow control limits how much work a connection can put on the server. It
protects the server when clients send too fast or break protocol limits.

For MQTT 5, `set_max_receive` limits how many QoS 1 and 2 publishes a
connection may have in flight. The server announces this as Receive
Maximum, and a client that exceeds it breaks the protocol. MQTT 3.1.1 has
no such announcement: the setting limits concurrent publish handling at
all QoS levels, with extra publishes waiting in order for a free slot.

Other limits pause reading rather than reject a packet. The connection
stops reading new packets while:

- the publish or protocol service is not ready;
- the response queue is full (`set_max_queue`);
- the packets being handled total `set_max_receive_size` bytes;
- a streaming payload has `set_max_payload_buffer_size` bytes your handler
  has not read yet.

For a streaming payload, reading resumes once the handler drains the buffer
to half that size.

During a read pause, the client's data waits in its TCP buffers. TCP flow
control eventually slows the client down. No read timeout runs during the
pause, because the server is causing the delay.

The other direction has its own limit. Publishes to a client use send
credit, just as on the client side. Set it with `set_max_send` or with
`max_send()` on the `ConnectAck`. In MQTT 5, it cannot exceed the client's
Receive Maximum.

Sends wait for credit and for write backpressure to clear, so a slow client
slows its senders down instead of letting them keep adding packets to the
write buffer.

Your services also control the connection through readiness. For example,
a publish handler can report that it is not ready when a downstream queue
is full. The connection stops reading until the handler is ready again.

Services whose readiness can change must notify ntex-mqtt through
`ReadyNotifier` in `bind()`. The [pipeline chapter](2-pipeline.md) explains
how.

### The protocol service

Provide a protocol service to handle subscriptions and other packets
besides publishes. A broker always needs one. Without a custom protocol
service, the server answers pings, `DISCONNECT` and `PUBREL`. It closes the
connection on anything else, including subscriptions.

The service receives a `ProtocolMessage` and returns a `ProtocolMessageAck`:

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

For a `SUBSCRIBE`, visit each topic filter. Call `confirm()` with the QoS
you grant, or call `fail()` to refuse it. Filters you leave alone are
refused.

In MQTT 5, each filter's `options()` gives the client's QoS, no-local,
retain-as-published and retain-handling choices. Use `packet()` to access
the whole packet. Unsubscribe works the same way, with `success()` and
`fail()`.

For routine packets, you can usually keep the default response with
`msg.ack()`. A `Ping` gets `PINGRESP`, and `PublishRelease` answers the
`PUBREL` that finishes a QoS 2 publish. `Disconnect` carries the client's
`DISCONNECT`; in MQTT 5, that packet includes a reason code and may update
the session expiry interval.

Enhanced authentication needs an explicit response. MQTT 5 delivers each
step as `Auth`; answer it with `auth.ack(packet)`.

Use `disconnect()` to turn any message into a disconnect. In MQTT 5,
`disconnect_with()` lets you choose the reason code.

The server handles protocol messages one at a time, in order. It buffers up
to 16.

### The control service

Use the control service to observe connection events and clean up when a
connection stops. It receives the same `Control` messages as the client
control service. You can return a packet to send before the connection
closes:

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

`Stop` arrives exactly once per accepted connection. Use it for cleanup,
such as removing a client from a registry, publishing its presence or
releasing resources. A graceful `DISCONNECT` from the client reaches the
protocol service before the connection stops.

When an MQTT 5 connection closes because of a protocol error, ntex-mqtt
sends the matching `DISCONNECT` reason code. You should usually return
`None` in that case. MQTT 3.1.1 has no server `DISCONNECT`, so return `None`.

### MQTT 5 features

MQTT 5 exposes more information through packet fields and properties.
For outgoing topic aliases, set `properties.topic_alias` on your publishes
and stay within the limit the client gave in `CONNECT`. For subscription
identifiers, read them from `SUBSCRIBE` and attach them to matching
publishes with `properties.subscription_ids`.

Some properties describe work your application must do. Will delay, for
example, is available in the `CONNECT` packet's last-will properties, but
your application must schedule the will.

This is the boundary between the protocol framework and a broker.
ntex-mqtt handles protocol rules, ordering, flow control and limits. Your
application matches subscriptions to publishes, stores retained messages
and manages persistent sessions, choosing how to store that data.

The [MQTT over WebSocket example] is a complete program with an MQTT 3.1.1
server behind an HTTP/1 WebSocket upgrade. It also includes a client that
publishes to the server. The [ntex-mqtt examples] include TLS, sessions,
routing and subscriptions.

[MQTT]: https://mqtt.org
[ntex-mqtt]: https://docs.rs/ntex-mqtt
[ntex-mqtt examples]: https://github.com/ntex-rs/ntex-mqtt/tree/main/examples
[MQTT over WebSocket example]: https://github.com/ntex-rs/examples/tree/main/protocols/mqtt
[`SharedCfg`]: https://docs.rs/ntex/latest/ntex/struct.SharedCfg.html
[`IoConfig`]: https://docs.rs/ntex/latest/ntex/io/struct.IoConfig.html
[`Pipeline`]: https://docs.rs/ntex/latest/ntex/struct.Pipeline.html
