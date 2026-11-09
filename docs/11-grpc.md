# Building gRPC Services and Clients

[gRPC] lets processes call each other through an API described in a `.proto`
file. Clients and servers written in any language can use that API. Calls
carry compact Protocol Buffers messages over HTTP/2, with many calls sharing
one connection.

[ntex-grpc] is a separate crate built on the ntex
[service model](1-service.md). It uses [`ntex-h2`] for HTTP/2. A gRPC client
is a service you call. A gRPC server is a service that the ntex
[server](5-server.md) runs for each connection.

ntex-grpc describes the protocol with ordinary traits. It does not use
derive macros or build scripts. Each message field knows how to encode
itself, and the message uses those implementations. A method type defines
its request path and its input and output messages. A client is generic
over the transport that carries its calls.

The `ntex-grpc-codegen` generator writes the boilerplate: a struct for each
message, a type for each method, and a typed client for each service. You
commit the output and read it like the rest of your code. Nothing is
generated during the build. The only macro is `#[server]`, which connects
the methods of a `.proto` service to the methods of your type.

These traits let you replace each part. You can use your own Rust types for
message fields, put a custom transport between a client and the network,
or handle raw, undecoded calls on the server.

The snippets come from the [gRPC examples][examples]. Each example is a
complete program. Its README includes run commands and a `grpcurl` session
you can try.

| Example | What it shows |
| --- | --- |
| [grpc-helloworld][helloworld-example] | the smallest service, server and client |
| [grpc-custom-types][custom-types-example] | your own Rust types for message fields |
| [grpc-routing-client][routing-example] | a custom client transport that routes calls to different servers |
| [grpc-server][server-example] | all four kinds of methods, several services, a fallback, server tuning |
| [grpc-tls][tls-example] | gRPC over TLS with rustls |

## Quick start: hello, gRPC

The helloworld example gives you a greeting service with one method. Clone
the examples repository and start the server:

```sh
git clone https://github.com/ntex-rs/examples
cd examples/protocols/grpc-helloworld
cargo run --bin grpc-helloworld-server
```

In a second terminal, call it:

```sh
cargo run --bin grpc-helloworld-client -- ntex
```

The client prints `Hello ntex!`. The server speaks standard gRPC, so you can
also call it with tools such as [grpcurl]:

```sh
grpcurl -plaintext -proto helloworld.proto -d '{"name": "ntex"}' \
    127.0.0.1:50051 helloworld.Greeter/SayHello
```

The service implementation is short:

```rust,no_run
#[server(crate::helloworld::Greeter)]
impl GreeterServer {
    #[method(SayHello)]
    async fn say_hello(&self, req: HelloRequest) -> HelloReply {
        let name = if req.name.is_empty() { "world" } else { &req.name };
        HelloReply {
            message: format!("Hello {name}!").into(),
        }
    }
}
```

The client builds a request, sets a timeout, and sends it:

```rust,no_run
let client = connect(ADDR);
let msg = HelloRequest { name: "ntex".into() };

let mut req = client.say_hello(&msg);
req.timeout(Duration::from_secs(3));
let reply = req.send().await?;
println!("{}", reply.message);
```

The generated file `src/helloworld.rs` defines the request and reply
messages, `HelloRequest` and `HelloReply`. It also defines the `Greeter`
service and the client's `say_hello` method.

To use ntex-grpc in your own project, add these dependencies:

```toml
[dependencies]
ntex = "4"
ntex-grpc = "4"
ntex-h2 = "4"
```

## gRPC in a nutshell

You do not need to know the wire protocol to use ntex-grpc. Its basic
structure helps you decide how to handle errors and retries.

A gRPC call is an HTTP/2 `POST` request. Its path identifies the RPC:
`/{package}.{Service}/{Method}`, such as `/helloworld.Greeter/SayHello`.
The headers identify gRPC with `content-type: application/grpc` and may
carry a timeout in `grpc-timeout`. Your own headers travel alongside them;
gRPC calls these *metadata*.

The body carries messages, each with a 5-byte prefix holding a compression
flag and the message length. The response sends headers, reply messages,
and then *trailers*. These carry the call's result in `grpc-status`, where
`0` means `OK`, and may include a `grpc-message` explaining an error.

That last part matters for error handling: a gRPC failure still has HTTP
status 200. The client must read the trailers to know how the call ended,
not just check the HTTP status.

All gRPC methods use this request structure. They differ in how many
messages each side sends:

| Kind | `.proto` | Client sends | Server sends |
| --- | --- | --- | --- |
| unary | `rpc M(Req) returns (Res)` | one message | one message |
| server streaming | `rpc M(Req) returns (stream Res)` | one message | any number |
| client streaming | `rpc M(stream Req) returns (Res)` | any number | one message |
| bidirectional | `rpc M(stream Req) returns (stream Res)` | any number | any number |

Each call uses one HTTP/2 stream. A connection can run many calls in
parallel without a slow call blocking the others.

## How ntex-grpc fits together

Generated Rust code connects your `.proto` definitions to the client and
server. The client passes calls through a transport; the server routes
incoming calls to your methods:

```text
           .proto ── ntex-grpc-codegen ──▶ src/helloworld.rs
                                        (messages, MethodDefs, typed client)

client:    GreeterClient<T> ─▶ Request ─▶ Transport ─▶ ntex-h2 pool ─▶ network

server:    network ─▶ ntex Server ─▶ GrpcServer ─▶ #[server] type ─▶ your method
```

Start with the message itself. [`NativeType`] reads and writes one field
value: an integer, a string, a nested message, or your own type. [`Message`]
encodes and decodes the whole message by calling `NativeType` for each
field. You normally use the generated implementations and only implement
`NativeType` yourself when a field needs a custom Rust type.

The generated method and service types describe the API. Each RPC is a
zero-sized type implementing [`MethodDef`], which supplies its request
path and input and output types. [`ServiceDef`] groups those methods under
the service's full name.

Finally, [`Transport`] carries the call. A generated client is generic
over this trait, so calling `send()` hands the request to whichever
transport you gave it. You can keep the generated API while replacing how
calls reach the server.

## From .proto to Rust

Run the code generator to turn your `.proto` definitions into Rust types.
You keep the generated file in your project, with no code generation during
builds.

### Running the code generator

Install `ntex-grpc-codegen` to get the `ntex-grpc` command. It uses
`protoc`, the Protocol Buffers compiler, to parse `.proto` files. Install
`protoc` through your package manager first. If it is outside your `PATH`,
set the `PROTOC` environment variable to its path.

```sh
cargo install ntex-grpc-codegen
```

The helloworld example defines its service in this `.proto` file:

```protobuf
syntax = "proto3";

package helloworld;

// The greeting service.
service Greeter {
  rpc SayHello (HelloRequest) returns (HelloReply);
}

// The request message with the user's name.
message HelloRequest {
  string name = 1;
}

// The reply message with the greeting.
message HelloReply {
  string message = 1;
}
```

Run the generator whenever the `.proto` file changes. The examples keep
this command in a `generate.sh` script next to the file:

```sh
ntex-grpc helloworld.proto helloworld.rs --out-dir ./src --include-dir ./ \
    --rustfmt-path ../../rustfmt.toml
```

The proto `package` determines the output file's name; the second argument
does not. Use separate output directories if you generate the same package
twice, for example with different options.

| Option | Effect |
| --- | --- |
| `--out-dir DIR` | where to write the generated file |
| `--include-dir DIR` | where to look for imported `.proto` files; can be repeated |
| `--map NAME=TYPE` | use your own Rust type for a field, see [using your own field types](#using-your-own-field-types) |
| `--rustfmt-path FILE` | rustfmt configuration used to format the output |
| `--optional-messages` | generate singular message fields as `Option<T>`, so you can tell an unset field from an empty one |
| `--open-enums` | generate enum fields as `i32`, so values your code does not know yet are kept |
| `--well-known-types` | generate the `google.protobuf` types instead of using `ntex_grpc::google_types` |

Commit the generated file so builds do not depend on `protoc`.

### What gets generated

Add the generated file to your project with `pub mod helloworld;`. You can
then use its types directly. Each protobuf `message` becomes a plain Rust
struct:

```rust,no_run
/// The request message with the user's name.
#[derive(Clone, PartialEq, Debug)]
pub struct HelloRequest {
    pub name: ::ntex_grpc::ByteString,
}
```

For the `Greeter` service, the generator also writes a unit struct named
`Greeter`. It implements [`ServiceDef`] with the full service name,
`helloworld.Greeter`. The typed client, `GreeterClient<T>`, is generic over
its transport and has one method per `rpc`.

Each RPC gets its own unit struct implementing [`MethodDef`]. For
`SayHello`, that definition looks like this:

```rust,no_run
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct GreeterSayHelloMethod;

impl ::ntex_grpc::MethodDef for GreeterSayHelloMethod {
    const NAME: &'static str = "SayHello";
    const PATH: ::ntex_grpc::ByteString =
        ::ntex_grpc::ByteString::from_static("/helloworld.Greeter/SayHello");
    type Input = HelloRequest;
    type Output = HelloReply;
    const CLIENT_STREAMING: bool = false;
    const SERVER_STREAMING: bool = false;
}
```

`MethodDef` ties a method to its request path and message types at compile
time. The client uses it to choose the request path and decode the reply.
The server uses it to decode requests and encode replies. If you pass the
wrong message to a method, compilation fails.

Each message struct also gets a [`Message`] implementation. It calls
`NativeType` for each field, passing the field's tag:

```rust,no_run
impl ::ntex_grpc::Message for HelloRequest {
    #[inline]
    fn write(&self, dst: &mut ::ntex_grpc::BytePages) {
        ::ntex_grpc::NativeType::serialize(
            &self.name,
            1,
            ::ntex_grpc::types::DefaultValue::Default,
            dst,
        );
    }
    // `read`, `merge_from` and `encoded_len` follow the same pattern
}
```

The generated implementation is straight-line code that the compiler can
inline. There is no runtime reflection or table of field descriptors.
Any type that implements `NativeType` works with this code, including your
own field types.

### How protobuf types map to Rust

The generator uses ntex types where they are available. String and byte
fields share memory with the received buffer without copying:

| Protobuf | Rust |
| --- | --- |
| `double`, `float` | `f64`, `f32` |
| `int32`, `sint32`, `sfixed32` | `i32` |
| `int64`, `sint64`, `sfixed64` | `i64` |
| `uint32`, `fixed32` | `u32` |
| `uint64`, `fixed64` | `u64` |
| `bool` | `bool` |
| `string` | `ByteString` |
| `bytes` | `Bytes` |
| `enum E` | `E`, a Rust enum (`i32` with `--open-enums`) |
| `M`, a message field | `M` (`Option<M>` with `--optional-messages`) |
| `optional T` | `Option<T>` |
| `repeated T` | `Vec<T>` |
| `map<K, V>` | `ntex_grpc::HashMap<K, V>` |
| `oneof name { .. }` | `Option<message::Name>`, an enum with one variant per field |
| `google.protobuf.Timestamp`, `Duration`, wrappers | the types in `ntex_grpc::google_types` |
| `google.protobuf.Empty` | `()` |

For example, this message uses enums, optional fields, a map, and a `oneof`:

```protobuf
enum Color {
  COLOR_UNSPECIFIED = 0;
  RED = 1;
  GREEN = 2;
}

message Shape {
  Color color = 1;
  optional uint32 weight = 2;
  map<string, uint64> labels = 3;
  oneof kind {
    double radius = 4;
    Side side = 5;
  }
}
```

The generated Rust types look like this:

```rust,no_run
pub struct Shape {
    pub color: Color,
    pub weight: Option<u32>,
    pub labels: ::ntex_grpc::HashMap<::ntex_grpc::ByteString, u64>,
    pub kind: Option<shape::Kind>,
}

/// Nested message and enum types in `Shape`.
pub mod shape {
    pub enum Kind {
        Radius(f64),
        Side(super::Side),
    }
}

#[repr(i32)]
pub enum Color {
    Unspecified = 0,
    Red = 1,
    Green = 2,
}
```

Two generator defaults deserve attention when your API changes over time.
By default, enums are closed: an unknown value decodes as the enum's
default variant. A sender using a newer `.proto` file can therefore send a
value your code cannot distinguish from the default. Use `--open-enums` to
keep the numeric value, then convert it with `Color::from_i32()` where
needed.

Singular message fields are also always present in the default Rust
output. An unset field decodes as the default message. Use
`--optional-messages` if your handler needs to distinguish an unset field
from an empty one.

### Using your own field types

Use `--map` when a field needs stricter rules than its protobuf type
provides. A `bytes` field might hold a 16-byte UUID, or a `string` might
hold only product codes. With the default generated types, each handler
must check those rules.

The option replaces a field's Rust type with your own. That type checks the
value once, during decoding:

```protobuf
message ReserveRequest {
  // Client-chosen request id, a 16-byte UUID.
  bytes request_id = 1;
  // Stock keeping unit, e.g. "BOOK-42".
  string sku = 2;
  uint32 quantity = 3;
}

message ReserveReply {
  bytes request_id = 1;
  string sku = 2;
  bool reserved = 3;
  uint32 remaining = 4;
}
```

```sh
ntex-grpc inventory.proto inventory.rs --out-dir ./src --include-dir ./ \
    --map request_id=crate::types::RequestId \
    --map sku=crate::types::Sku
```

The left side of `--map` is a field path. A leading dot makes it fully
qualified, as in `.inventory.ReserveRequest.sku`. Without the dot, it
matches a suffix. Here, `sku` covers both messages, while
`ReserveRequest.sku` covers only the request.

The right side is a Rust path. The generator copies it into the output
unchanged:

```rust,no_run
pub struct ReserveRequest {
    pub request_id: crate::types::RequestId,
    pub sku: crate::types::Sku,
    pub quantity: u32,
}
```

The wire format stays the same. Other gRPC clients and servers still send
plain `bytes` and `string` fields. On the Rust side, your type decides which
values are valid. Decoding fails for anything else.

The server answers `INVALID_ARGUMENT` before calling your handler if an id
or SKU is malformed. Your handler receives a `ReserveRequest` whose field
types enforce those rules.

### Writing a custom field type

Implement [`NativeType`] to use your own type in a message field. The type
also needs `Default`, `PartialEq` and `Debug` because the generated structs
derive traits that require them.

| Item | What it does |
| --- | --- |
| `const TYPE: WireType` | the protobuf wire type: `LengthDelimited` for `bytes`, `string` and messages, `Varint` for integers and `bool`, `ThirtyTwoBit` or `SixtyFourBit` for fixed-width numbers |
| `value_len(&self)` | the encoded length of the value, without the tag and length prefix |
| `merge(&mut self, src)` | decode a value; for length-delimited types `src` holds exactly the value, the length prefix is already removed |
| `encode_value(&self, dst)` | write the value, without the tag and length prefix |
| `is_default(&self)` | `true` if the value is the default, so the field is left out of the message |

The custom-types example uses this request id. Its default value is a nil
UUID. Like an empty `bytes` field, that value is left out of the encoded
message:

```rust,no_run
use ntex_grpc::{BytePages, Bytes, DecodeError, NativeType, WireType};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct RequestId(Uuid);

impl NativeType for RequestId {
    const TYPE: WireType = WireType::LengthDelimited;

    fn value_len(&self) -> usize {
        16
    }

    /// `src` holds exactly the field value, the length prefix is already
    /// removed.
    fn merge(&mut self, src: &mut Bytes) -> Result<(), DecodeError> {
        let id =
            Uuid::from_slice(src).map_err(|_| DecodeError::new("request id must be 16 bytes"))?;
        *self = Self(id);
        Ok(())
    }

    fn encode_value(&self, dst: &mut BytePages) {
        dst.extend_from_slice(self.0.as_bytes());
    }

    fn is_default(&self) -> bool {
        self.0.is_nil()
    }
}
```

A wrapper around a built-in type can delegate wire-format handling to that
type. It can then add its own validation. Here, `Sku` lets `ByteString`
check for valid UTF-8 before validating the string:

```rust,no_run
impl NativeType for Sku {
    const TYPE: WireType = WireType::LengthDelimited;

    fn value_len(&self) -> usize {
        self.0.value_len()
    }

    fn merge(&mut self, src: &mut Bytes) -> Result<(), DecodeError> {
        let mut s = ByteString::default();
        s.merge(src)?;
        *self = Self::try_from(s).map_err(|e| DecodeError::new(e.to_string()))?;
        Ok(())
    }

    fn encode_value(&self, dst: &mut BytePages) {
        self.0.encode_value(dst);
    }

    fn is_default(&self) -> bool {
        self.0.is_empty()
    }
}
```

The custom type still has to obey protobuf's encoding rules. For a
length-delimited field, `encode_value()` must write exactly `value_len()`
bytes: the length prefix has already been written using that value. A
mismatch corrupts the message.

For types that are not length-delimited, override `encoded_len(tag)` as
well. Its default implementation adds a length prefix, which is correct
only for `LengthDelimited`. A `Varint` newtype, such as a `u64` id, must
return `key_len(tag) + value_len()`, as the built-in integer types do.
Otherwise, encoded messages get the wrong size.

Validation should return an error from `merge`, never panic. That error
becomes `INVALID_ARGUMENT` on the server or `InvalidResponse` on the client.
Also keep `is_default` consistent with protobuf: other implementations
treat a missing field as empty, zero or `false`, so your default must mean
the same thing.

## Calling services

Call a generated client method to build a request. Each method corresponds
to one `rpc`. You can configure the request before calling `send()`;
building it sends nothing:

```rust,no_run
use std::time::Duration;

use grpc_helloworld::{ADDR, connect, helloworld::HelloRequest};

#[ntex::main]
async fn main() {
    let client = connect(ADDR);
    let msg = HelloRequest { name: "ntex".into() };

    let mut req = client.say_hello(&msg);
    req.timeout(Duration::from_secs(3));

    match req.send().await {
        // `Response` derefs to the reply message
        Ok(res) => println!("{}", res.message),
        Err(err) => eprintln!("call failed with {:?}: {err}", err.status()),
    }
}
```

The request borrows the client and message. Nothing is copied until the
message is encoded. Settings such as the timeout, headers or compression
apply only to this call.

### Connecting to a server

Use [`ntex_grpc::client::Client`] to connect through an [`ntex-h2`]
connection pool. It adds a thin gRPC layer over the pool:

```rust,no_run
use ntex::SharedCfg;
use ntex_grpc::client::Client;
use ntex_h2::client as h2;

pub fn connect(addr: impl Into<String>) -> GreeterClient<Client> {
    let pool = h2::Client::builder(addr.into()).build(SharedCfg::new("GRPC-CLIENT"));
    GreeterClient::new(Client::new(pool))
}
```

Building the pool does not touch the network. The first call opens a
connection. Later calls share it, each on its own HTTP/2 stream.

When a connection closes, the pool opens a new one for the next call that
needs it. You do not need to manage connections yourself.

Cloning a `Client` is cheap, and its clones share the pool. Build one per
server. Clone it into each typed client that talks to that server:

```rust,no_run
let pool = h2::Client::builder(addr.into()).build(SharedCfg::new("GRPC-CLIENT"));
let client = ntex_grpc::client::Client::new(pool);
let echo = EchoClient::new(client.clone());
let stats = StatsClient::new(client);
```

The pool builder controls the connections:

| Method | Effect |
| --- | --- |
| `connector(..)` | how to connect, e.g. through a TLS connector, see [running over TLS](#running-over-tls) |
| `secure(true)` | send the `https` scheme; use it together with TLS |
| `connect_timeout(..)` | time limit for the TCP connect and the TLS handshake |
| `minconn(..)` / `connection_limit(..)` | how many connections to keep open and how many to allow |
| `max_streams(..)` | how many calls can share one connection |
| `lifetime(..)` | retire connections after a while, e.g. to pick up DNS changes |

For HTTP/2 settings such as window sizes and the ping interval, use
`ntex_h2::ServiceConfig`. Add it to the `SharedCfg` you pass to `build()`.
This works the same way as on the [server](#tuning-the-server).

### What happens during a call

The request builder keeps your message separate from the work of sending
it. A [`Request`] holds the transport and input message, while its
[`RequestContext`] holds the headers, timeout and limits. Calling `send()`
passes them through the [`Transport`] trait. The transport handles the
HTTP/2 stream and returns a [`Response`] containing the decoded reply,
headers and trailers:

```rust,no_run
pub trait Transport<T: MethodDef, St = ()> {
    type Error;

    async fn request(
        &self,
        args: &T::Input,
        ctx: &mut RequestContext<St>,
    ) -> Result<Response<T>, Self::Error>;
}
```

The trait is generic over the method `T`. The transport therefore knows
the method's `PATH`, `Input` and `Output` at compile time.

The built-in gRPC `Client` delegates to the HTTP/2 pool client,
`ntex_h2::client::Client`. Once it has a connection, it uses that
connection's `ntex_h2::client::SimpleClient` to do the following:

1. Check for invalid request headers. If any were set, fail with
   `InvalidRequest` without sending anything.
2. Encode the input with its `Message` implementation. Compress it if
   requested, then add the 5-byte message prefix.
3. Open a stream with `POST T::PATH`. Include the request's headers and
   the gRPC headers: `content-type: application/grpc`, `te: trailers`, and
   `grpc-timeout` if a timeout is set.
4. Send the message and close the stream's sending side.
5. Read the response headers, message and trailers. Check the HTTP status,
   content type, message size and `grpc-status`.
6. Decode the message as `T::Output` and return a `Response`.

The timeout covers the whole call, including the wait for a connection.
When it runs out, the transport resets the stream with `CANCEL`. The call
fails with `DeadlineExceeded`.

To pass per-call state to the transport, use `send_with(&state)`. The
transport reads the reference with `ctx.request_state()`. Calling `send()`
is equivalent to `send_with(&())`.

### Headers, compression and message limits

Set request headers when you need to send metadata, such as authentication
tokens or trace ids. Configure them before sending the request:

```rust,no_run
use ntex_grpc::encode_binary_header;

let mut req = client.say_hello(&msg);
req.header("x-request-id", "42")
    .header("trace-bin", encode_binary_header(&trace_id))
    .timeout(Duration::from_secs(3));

let (reply, headers, trailers) = req.send().await?.into_parts();
```

Use `header()` to replace a header. Use `append_header()` to add another
value. By gRPC convention, names ending in `-bin` carry binary values.
Encode those with [`encode_binary_header()`] and decode received values
with `decode_binary_header()`.

A successful [`Response`] derefs to the reply message. Read its metadata
with `headers()` and `trailers()`. Use `into_parts()` to take the message,
headers and trailers apart.

The client sets protocol headers itself, including `content-type`, `te`,
`grpc-timeout` and `grpc-encoding`. It ignores any values you set for them.
If you set an invalid header name or value, the call fails with
`InvalidRequest` without sending it.

For large messages, you can also enable optional compression with the
`compression` feature:

```toml
ntex-grpc = { version = "4", features = ["compression"] }
```

```rust,no_run
use ntex_grpc::Compression;

req.compression(Compression::Gzip); // or Compression::Zstd
```

Compression costs CPU on both sides. Use it for large, repetitive messages
and leave it off for small ones. Empty messages are always sent
uncompressed. If the server does not support the encoding, it fails the
call with `UNIMPLEMENTED`.

Whether or not you compress, message limits protect the client from
oversized replies. On `Client`,
`max_message_size()` limits received messages to 4 MiB by default. Use
`max_send_message_size()` to limit sent messages. You can override both
limits on a request for a single call.

### Streaming calls

Use streaming methods when either side needs to send more than one
message. These methods return a `StreamRequest`, which you configure like
a unary request. You open it through the [`StreamTransport`] trait.

A [`SendStream`] sends messages. A [`RecvStream`] receives them:

```rust,no_run
// server streaming: one request, a stream of replies
let mut rx = echo.server_streaming_echo(&msg).open().await?;
while let Some(reply) = rx.recv().await {
    println!("{}", reply?.message);
}

// client streaming: send while `send()` waits for the reply
let tx = SendStream::new();
let (res, sent) = join(echo.client_streaming_echo(&tx).send(), async {
    for word in ["one", "two", "three"] {
        tx.send(&echo_request(word)).await?;
    }
    // the end of the stream, the server answers now
    tx.close();
    Ok::<_, StreamError>(())
})
.await;
sent?;
println!("{}", res?.message);

// bidirectional streaming: send and receive at the same time
let (tx, mut rx) = echo.bidi_streaming_echo().open().await?;
```

The example uses `ntex::util::join` so sending messages and waiting for the
reply can both make progress. When the server reads until the stream ends,
`tx.close()` tells it to finish and reply. Awaiting `send()` before sending
those messages would leave both sides waiting for each other.

After a stream ends, `rx.status()` returns its final `grpc-status`.

### Writing your own transport

Write a custom transport when you need behavior that applies to every
call. You can add authentication headers, record metrics or logs, retry
calls, or route them to different servers.

Generated clients are generic over their transport. Any type implementing
`Transport` can replace `Client`. For streaming methods, implement
`StreamTransport` too.

A custom transport usually wraps a built-in one. It adjusts the request
context, then delegates the call:

```rust,no_run
use ntex::Error;
use ntex_grpc::MethodDef;
use ntex_grpc::client::{Client, ClientError, RequestContext, Response, Transport};

/// Adds an `authorization` header to every call.
#[derive(Clone)]
pub struct WithToken {
    inner: Client,
    token: String,
}

impl<M: MethodDef, St> Transport<M, St> for WithToken {
    type Error = Error<ClientError>;

    async fn request(
        &self,
        args: &M::Input,
        ctx: &mut RequestContext<'_, St>,
    ) -> Result<Response<M>, Self::Error> {
        ctx.header("authorization", self.token.as_str());
        Transport::<M, St>::request(&self.inner, args, ctx).await
    }
}
```

Pass this wrapper to `GreeterClient::new(WithToken { .. })` to send the
token with every call. Keep the implementation generic over the request
state `St`. This preserves support for both `send()` and `send_with()`.

Implement `StreamTransport` the same way for streaming calls. Its `open()`
returns a [`StreamCall`] representing the call's HTTP/2 stream. Only the
built-in transports can create one. A custom streaming transport must
finish by calling a built-in transport's `open()`.

Generated clients own their transport. To share one between typed clients,
make it cheap to clone. Keep shared parts behind an `Rc` or in the pool,
as `Client` does.

### Routing calls to different backends

A routing transport lets one client setup reach services on different
servers. The routing example hosts `shop.Catalog` and `shop.Orders` on
separate backends. Its `Router` transport chooses a backend from the
service name in `M::PATH`:

```rust,no_run
use std::{collections::HashMap, rc::Rc};

use ntex::{Error, SharedCfg};
use ntex_grpc::client::{
    CallFailure, Client, ClientError, RequestContext, Response, StreamCall, StreamTransport,
    Transport,
};
use ntex_grpc::{GrpcStatus, MethodDef};
use ntex_h2::client as h2;

/// Routes calls by service name, e.g. `shop.Catalog`, to a backend client.
#[derive(Clone, Default)]
pub struct Router {
    routes: Rc<HashMap<&'static str, Client>>,
}

impl Router {
    /// Sends the calls of `service` to the server at `addr`.
    pub fn route_to(mut self, service: &'static str, addr: impl Into<String>) -> Self {
        let pool = h2::Client::builder(addr.into()).build(SharedCfg::new("GRPC-ROUTER"));
        Rc::make_mut(&mut self.routes).insert(service, Client::new(pool));
        self
    }

    /// The backend for the service of method `M`.
    fn backend<M: MethodDef>(&self) -> Result<&Client, Error<ClientError>> {
        let path = M::PATH;
        let service = service_name(&path);
        self.routes.get(service).ok_or_else(|| {
            // Nothing is sent and trying again fails the same way.
            Error::from(ClientError::InvalidRequest(CallFailure::new(
                GrpcStatus::Unimplemented,
                format!("no backend for {service}"),
            )))
        })
    }
}

/// The service part of a request path: `/shop.Catalog/GetItem` gives
/// `shop.Catalog`.
pub fn service_name(path: &str) -> &str {
    path.trim_start_matches('/')
        .split_once('/')
        .map_or("", |(service, _)| service)
}

impl<M: MethodDef, St> Transport<M, St> for Router {
    type Error = Error<ClientError>;

    async fn request(
        &self,
        args: &M::Input,
        ctx: &mut RequestContext<'_, St>,
    ) -> Result<Response<M>, Self::Error> {
        Transport::<M, St>::request(self.backend::<M>()?, args, ctx).await
    }
}

/// Only the built-in transports can open a `StreamCall`, so pick a backend
/// and let it open the stream.
impl<M: MethodDef, St> StreamTransport<M, St> for Router {
    async fn open(
        &self,
        ctx: &mut RequestContext<'_, St>,
    ) -> Result<StreamCall, Error<ClientError>> {
        StreamTransport::<M, St>::open(self.backend::<M>()?, ctx).await
    }
}
```

Both typed clients use the same router. Its clones share the routes and
connection pools. Use the generated `ServiceDef::NAME` constants as route
names. If a service is renamed in the `.proto` file, these references fail
to compile:

```rust,no_run
let router = Router::default()
    .route_to(Catalog::NAME, "127.0.0.1:50053")
    .route_to(Orders::NAME, "127.0.0.1:50054");
let catalog = CatalogClient::new(router.clone());
let orders = OrdersClient::new(router);
```

The router uses the same error type as the built-in transports. Callers
handle a missing route like any other failed call.

### Errors, deadlines and retries

A lost reply does not tell you whether the server handled the request.
Before retrying, check what the error says about that: repeating a greeting
is harmless, but repeating an order might not be.

The built-in transports return `ntex::Error<ClientError>`. It derefs to
[`ClientError`], so you can match on `&*err`. Use `err.status()` for a gRPC
status code on any variant. Use `err.message()` for a description of the
cause.

The error variant tells you what is known about the request:

| Variant | Did the server see the request? | What you can do |
| --- | --- | --- |
| `Status` | yes | apply the rules for the status code |
| `InvalidRequest` | no | fail the call; retrying fails the same way |
| `NotProcessed` | no | retry at once, for any method |
| `Connection` | maybe, see `request_sent` | retry, ideally on another connection |
| `StreamFailed` | maybe | retry if the method is idempotent |
| `NotGrpc` | not by a gRPC server | check the address, or back off on HTTP 429, 502, 503 and 504 |
| `InvalidResponse` | yes | fail the call; asking again gets the same answer |
| `DeadlineExceeded` | maybe | the time for the call is up |

Only `Status` carries a status from the server. It also carries the
server's trailers.

`NotProcessed` means the request never reached your service. It covers
connections that were already closing when the call started and streams
the server refused.

For an idempotent call, you can write a retry loop like this:

```rust,no_run
use ntex_grpc::client::ClientError;

let mut attempts = 0;
let reply = loop {
    let mut req = client.say_hello(&msg);
    req.timeout(Duration::from_secs(3));
    match req.send().await {
        Ok(res) => break res.into_inner(),
        Err(err) => {
            let retry = match &*err {
                ClientError::NotProcessed(_) => true,
                ClientError::Connection { request_sent, .. } => !request_sent,
                _ => false,
            };
            attempts += 1;
            if !retry || attempts == 3 {
                return Err(err);
            }
        }
    }
};
```

Retries also need a time limit. Set a timeout on every call; without one,
a stalled server can hold the call and anything waiting for it as long as
the connection lives.

The client sends the value from `timeout()` in the `grpc-timeout` header,
rounding up to the precision the gRPC format allows. The client enforces
its own deadline, including time spent waiting for a connection. The
server uses the header to limit service execution and answers
`DEADLINE_EXCEEDED` if that limit runs out. These are separate timers, not
a synchronized clock. A zero client timeout fails at once without sending
anything.

Streaming calls fail in the same ways. The stage of the call determines
where you receive the error:

- `open()` returns errors from opening the stream.
- `recv()` returns a [`StreamError`] if the stream fails later.
- `rx.status()` gives the final status.

## Serving gRPC

On the server side, your main job is to implement one async method per
`rpc`. The `#[server]` attribute turns your type into an ntex service, so
you do not have to decode packets or dispatch method names yourself.

Two layers sit in front of it. [`GrpcServer`] speaks the gRPC protocol and
routes calls by service name. The ntex [`Server`](5-server.md) accepts
connections, optionally runs TLS, and hands each connection to
`GrpcServer`.

### Implementing a service

Implement a service with async methods, then register it with the server.
This is the complete helloworld server:

```rust,no_run
use std::convert::Infallible;

use ntex::{ServiceFactory, SharedCfg, server::Server};
use ntex_grpc::server;

#[derive(Clone, Debug, Default)]
pub struct GreeterServer;

#[server(crate::helloworld::Greeter)]
impl GreeterServer {
    #[method(SayHello)]
    async fn say_hello(&self, req: HelloRequest) -> HelloReply {
        let name = if req.name.is_empty() { "world" } else { &req.name };
        HelloReply {
            message: format!("Hello {name}!").into(),
        }
    }
}

/// `GrpcServer` creates the service for every connection.
impl ServiceFactory<(), server::ServerRequest> for GreeterServer {
    type Res = server::ServerResponse;
    type Error = server::ServerError;
    type Service = GreeterServer;
    type InitError = Infallible;

    async fn create(&self, _: &()) -> Result<Self::Service, Self::InitError> {
        Ok(self.clone())
    }
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    Server::builder()
        .bind("grpc", "127.0.0.1:50051", SharedCfg::new("GRPC"), async |_| {
            server::GrpcServer::new(GreeterServer)
        })?
        .run()
        .await
}
```

Pass the generated service definition to `#[server(path)]`. Mark each
method with `#[method(Name)]`, using the name from the `.proto` file. The
attribute checks argument and return types against the generated
`MethodDef` at compile time. A misspelled method name or wrong message type
fails to compile.

The attribute implements `Service<(), ServerRequest>` for your type. It
also implements `NamedService`. The rest is ordinary Rust.

The signature of a method depends on its kind:

| Kind | Arguments | Returns |
| --- | --- | --- |
| unary | `req: Input` or `req: Request<Input>` | `Output`, `Response<Output>`, `Result<Output, E>` with `E: Into<Output>`, or `Result<Response<Output>, E>` with `E: Into<ServerError>` |
| server streaming | `req`, `tx: SendStream<Output>` | `()` or `Result<(), E>` with `E: Into<ServerError>` |
| client streaming | `rx: RecvStream<Input>` | as unary |
| bidirectional | `rx: RecvStream<Input>`, `tx: SendStream<Output>` | as server streaming |

Use `Request<Input>` when you need request headers. It derefs to the
message, so you can still access the fields:

```rust,no_run
#[method(UnaryEcho)]
async fn unary_echo(&self, req: Request<EchoRequest>) -> EchoReply {
    let prefix = req
        .headers
        .get(PREFIX_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    EchoReply {
        message: format!("{prefix}{}", req.message.message).into(),
    }
}
```

`GrpcServer` calls `create()` for each connection to make a service
instance. State stored in that returned service belongs to the connection.
If you need to share it more widely, create it outside `create()` and
clone a handle into each instance.

For state shared by one worker, use the closure passed to `bind()`. It runs
for each worker, so an `Rc` is enough. The closure may run again after a
worker restarts; keep the setup safe to repeat. For a counter or data store
shared by the whole server, use an `Arc` shared by all clones, because
workers run on different threads.

Import the attribute with `use ntex_grpc::server;`. This imports both the
`server` module and the attribute. An import such as
`use ntex_grpc::server::{self, ..}` brings in only the module. If you use
that form, the error says that `NamedService` is not implemented.

### Streaming methods

Streaming methods let you send or receive messages throughout a call. Use
`RecvStream` to receive messages and `SendStream` to send them. A
server-streaming method receives one request and a sender:

```rust,no_run
#[method(ServerStreamingEcho)]
async fn server_streaming_echo(
    &self,
    req: EchoRequest,
    tx: SendStream<EchoReply>,
) -> Result<(), ServerError> {
    if req.repeat > MAX_REPEAT {
        return Err(ServerError::new(
            GrpcStatus::InvalidArgument,
            encode_grpc_message(&format!("repeat must be at most {MAX_REPEAT}")),
            None,
        ));
    }
    let reply = EchoReply { message: req.message };
    for _ in 0..req.repeat {
        tx.send(&reply).await?;
    }
    Ok(())
}
```

A client-streaming method reads messages until the client closes its side.
It then returns one reply:

```rust,no_run
#[method(ClientStreamingEcho)]
async fn client_streaming_echo(&self, mut rx: RecvStream<EchoRequest>) -> EchoReply {
    let mut messages = Vec::new();
    while let Some(Ok(req)) = rx.recv().await {
        messages.push(req.message);
    }
    EchoReply {
        message: messages.join(" ").into(),
    }
}
```

A bidirectional method gets both streams. It can reply to each message as
it arrives:

```rust,no_run
#[method(BidiStreamingEcho)]
async fn bidi_streaming_echo(
    &self,
    mut rx: RecvStream<EchoRequest>,
    tx: SendStream<EchoReply>,
) -> Result<(), ServerError> {
    while let Some(req) = rx.recv().await {
        let req = req?;
        tx.send(&EchoReply { message: req.message }).await?;
    }
    Ok(())
}
```

Use `?` to return early after a failed send or receive. If the client goes
away, the next `send()` fails. When the method returns, the call ends. Its
result becomes the call's `grpc-status`.

### Reporting errors to clients

Use your method's return value to report errors to clients. A
`ServerError` sets the call's `grpc-status` and `grpc-message`. Its optional
`HeaderMap` adds trailers. Build the error with `ServerError::new()`.

Pass error text through [`encode_grpc_message()`]. gRPC requires
`grpc-message` to be percent-encoded.

Streaming methods can return `Result<(), ServerError>`, as shown above.
For unary and client-streaming methods, return
`Result<Response<Output>, ServerError>` to fail the call with a status.
Those methods return a reply, so the return type must distinguish an error
status from an error encoded in the reply.

A successful `Response` can carry extra trailers. You can turn a plain
message into a `Response` with `.into()`:

```rust,no_run
use ntex::http::header::HeaderValue;
use ntex_grpc::server::{Response, ServerError};

#[method(Reserve)]
async fn reserve(
    &self,
    req: ReserveRequest,
) -> Result<Response<ReserveReply>, ServerError> {
    let Some(seats) = self.store.free_seats(&req.show) else {
        return Err(ServerError::new(
            GrpcStatus::NotFound,
            encode_grpc_message(&format!("no show {}", req.show)),
            None,
        ));
    };
    let reply = self.store.reserve(&req)?; // a store error converts into ServerError
    Ok(Response {
        message: reply,
        headers: vec![(SEATS_LEFT, HeaderValue::from(seats - 1))],
    })
}
```

The trailers in `Response::headers` follow `grpc-status: 0`. The server
ignores any `grpc-status` or `grpc-message` among them. A `Response`
therefore always represents a successful call. The client reads these
trailers with `trailers()` on its response.

A method returning `Result<Output, E>` **always ends with `OK`**. Its error
type converts into a reply message through `E: Into<Output>`. The error
must therefore be part of the reply.

Use this form when your protocol models errors as reply data. For example,
a `oneof` can represent success or failure:

```protobuf
message ReserveReply {
  oneof outcome {
    Reservation reserved = 1;
    Problem problem = 2;
  }
}
```

```rust,no_run
impl From<Problem> for ReserveReply {
    fn from(problem: Problem) -> Self {
        ReserveReply {
            outcome: Some(reserve_reply::Outcome::Problem(problem)),
        }
    }
}

#[method(Reserve)]
async fn reserve(&self, req: ReserveRequest) -> Result<ReserveReply, Problem> {
    let reservation = self.store.reserve(&req)?;
    Ok(ReserveReply {
        outcome: Some(reserve_reply::Outcome::Reserved(reservation)),
    })
}
```

An error in the reply gives clients a typed `Problem` rather than a status
code and string. Use a gRPC status for generic failures, such as a missing
resource or denied permission. Use a `oneof` when clients need to act on
the details.

The server handles these errors before your method runs:

| Situation | Status |
| --- | --- |
| unknown service or method | `UNIMPLEMENTED` |
| request message that cannot be decoded, including a custom field type that rejects its value | `INVALID_ARGUMENT` |
| request message larger than `max_message_size` | `RESOURCE_EXHAUSTED` |
| compressed request in an encoding the server does not support | `UNIMPLEMENTED` |
| deadline from `grpc-timeout` runs out | `DEADLINE_EXCEEDED` |

### Hosting several services

Register several services on one `GrpcServer` to share a listener. It
routes calls by the service name in the request path:

```rust,no_run
pub fn grpc_server(calls: Arc<AtomicU64>) -> GrpcServer {
    GrpcServer::new(EchoServer {
        calls: calls.clone(),
    })
    .service(StatsServer { calls })
    .default_service(Fallback)
    .max_message_size(MAX_MESSAGE_SIZE)
}
```

| Method | Effect |
| --- | --- |
| `new(service)` / `service(service)` | add a `#[server]` service under its `NamedService::NAME` |
| `service_named(name, service)` | add a service under another name, e.g. a hand-written one |
| `default_service(service)` | take the calls for every service that is not added |

Without a default service, calls to an unknown service fail with
`UNIMPLEMENTED`. Registering the same name twice panics at server startup,
before any calls arrive.

Write the default service and `service_named` services as plain ntex
services of `ServerRequest`. They receive the call before decoding, with
its service and method names, request headers and encoded payload.

Return a `ServerResponse` with an encoded reply, or a `ServerError` with
any status. These hand-written services handle only unary calls.

```rust,no_run
#[derive(Clone, Copy, Debug, Default)]
pub struct Fallback;

impl Service<(), server::ServerRequest> for Fallback {
    type Res = server::ServerResponse;
    type Error = ServerError;

    async fn call(
        &self,
        req: server::ServerRequest,
        _: Ctx<'_, Self, ()>,
    ) -> Result<Self::Res, Self::Error> {
        let msg = format!(
            "unknown service {}, this server has echo.Echo and echo.Stats",
            req.service
        );
        Err(ServerError::new(
            GrpcStatus::Unimplemented,
            encode_grpc_message(&msg),
            None,
        ))
    }
}
```

Implement `ServiceFactory` for `Fallback` too, as you would for a
`#[server]` type.

### Tuning the server

Start with message limits when you need to bound the requests your service
accepts. `GrpcServer::max_message_size(size)` defaults to 4 MiB and checks
the size again after decompression. A larger request fails the call with
`RESOURCE_EXHAUSTED`. To limit replies as well, set
`max_send_message_size(size)`.

For connection settings below gRPC, configure HTTP/2. `GrpcServer` reads
these settings from its listener's configuration. Add an
`ntex_h2::ServiceConfig` to the `SharedCfg` you pass to `bind()`:

```rust,no_run
use ntex::{SharedCfg, time::Seconds};
use ntex_h2 as h2;

pub fn server_cfg() -> SharedCfg {
    SharedCfg::new("GRPC")
        .add(
            h2::ServiceConfig::new()
                // parallel calls per connection
                .set_max_concurrent_streams(256)
                // per-call and per-connection receive windows
                .set_initial_window_size(256 * 1024)
                .set_initial_connection_window_size(1024 * 1024)
                // close connections that stop answering pings
                .set_ping_timeout(Seconds(20))
                .set_handshake_timeout(Seconds(5)),
        )
        .into()
}

Server::builder()
    .bind("grpc", ADDR, server_cfg(), async move |_| grpc_server(calls.clone()))?
    .run()
    .await
```

Start with the defaults and adjust them for your traffic:

| Setting | Default | Why change it |
| --- | --- | --- |
| `set_max_concurrent_streams` | 256 | the number of calls a client can run in parallel on one connection |
| `set_initial_window_size` | 65,535 | larger windows speed up large messages and streams |
| `set_initial_connection_window_size` | 4 MiB | the receive window shared by all calls on a connection |
| `set_ping_timeout` | 10 seconds | detect dead connections; zero disables pings |
| `set_handshake_timeout` | 5 seconds | how long a new connection has to complete the HTTP/2 handshake |

With the `compression` feature, the server accepts gzip and zstd requests.
It uses the request's encoding to compress the reply, unless the client's
`grpc-accept-encoding` header rules that out.

### Running over TLS

Put a TLS service in front of `GrpcServer` to serve gRPC over TLS. The
server accepts any [`Io`](7-io.md), but speaks only HTTP/2, as gRPC requires.
The TLS acceptor runs first and passes the decrypted stream to it:

```rust,no_run
use ntex::io::Io;
use ntex::tls::rustls::TlsAcceptor;
use ntex::Service;

pub fn grpc_tls_service(
    config: Arc<ServerConfig>,
) -> impl Service<(), Io, Res = (), Error = Box<dyn std::error::Error>> {
    ntex::service(TlsAcceptor::new(config))
        .map_err(Box::<dyn std::error::Error>::from)
        .and_then(grpc_server())
}
```

On the client, give the pool a TLS connector. Set `secure(true)` so
requests carry the `https` scheme:

```rust,no_run
use ntex::tls::rustls::TlsConnector;

pub fn connect(addr: impl Into<String>, config: ClientConfig) -> GreeterClient<Client> {
    let pool = h2::Client::builder(addr.into())
        .connector(TlsConnector::new(config))
        .secure(true)
        .build(SharedCfg::new("GRPC-TLS-CLIENT"));
    GreeterClient::new(Client::new(pool))
}
```

Both sides must offer `h2` in ALPN. Set
`config.alpn_protocols = vec![b"h2".to_vec()]`. Many gRPC clients refuse a
server that does not select it.

Connect to a host name that matches the server certificate, such as
`localhost:50056`. See the [TLS chapter](10-tls.md) for certificates, trust
roots and other TLS backends.

Without TLS, ntex-grpc uses HTTP/2 with *prior knowledge*. The client sends
the HTTP/2 preface immediately, without an HTTP/1.1 upgrade. gRPC clients
expect this for plaintext connections. Use `-plaintext` with tools such as
`grpcurl` to call a server without TLS.

## Testing your services

Use the ntex test server to test your service with real gRPC calls. Clients
and servers are ordinary ntex services. Start the gRPC server on a random
port, then connect a generated client:

```rust,no_run
use ntex::server::TestServerBuilder;

#[ntex::test]
async fn unary_echo_reads_request_headers() {
    let calls = Arc::new(AtomicU64::new(0));
    let srv = TestServerBuilder::new(async move |_| grpc_server(calls.clone()))
        .config(server_cfg())
        .start();

    let (echo, _) = connect(srv.addr().to_string());
    let msg = echo_request("hello");
    let mut req = echo.unary_echo(&msg);
    req.header(PREFIX_HEADER, "> ").timeout(Seconds(3));
    assert_eq!(req.send().await.unwrap().message, "> hello");
}
```

Pass the binary's `SharedCfg` to `TestServerBuilder::config()` to test with
the production HTTP/2 settings. Keep the timeout on every test call: a
broken server should fail the test rather than hang it.

Once the successful call works, exercise the routing failures too.
Implement `MethodDef` by hand with a `PATH` that the server does not have,
then send it with `ntex_grpc::client::Request::new`. Check for `UNIMPLEMENTED`
or your fallback's answer.

Custom field types need both local and wire-compatibility tests. For a
local round trip, encode with `Message::write`, decode with `Message::read`,
and compare the messages. Also check that `merge` returns an error for
invalid input. To test compatibility, generate the same `.proto` with and
without `--map`, then use the plain client to call the custom-types server.
That exercises the wire format other implementations see.

## Troubleshooting

Match the symptom to a likely cause before changing your client or server
configuration.

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| The code generator cannot find `protoc` | `protoc` is not installed or not in `PATH` | install it, or set `PROTOC` to its path |
| The generated file has an unexpected name | the file is named after the proto `package` | use the package name in `mod` declarations |
| "the trait `NamedService` is not implemented" | the `server` attribute is not imported | use `use ntex_grpc::server;`, not `use ntex_grpc::server::{self}` |
| A client-streaming call never returns | `send()` was awaited before the messages were sent | send and await the reply at the same time, with `join` |
| Every call fails with `UNIMPLEMENTED` | the client and the server disagree on the package or service name | compare the request path with `ServiceDef::NAME` on both sides |
| Calls fail with `INVALID_ARGUMENT` "Cannot decode grpc message" | the `.proto` files differ, or a custom field type rejected a value | regenerate from the same `.proto`; check your `merge` |
| Calls fail with `RESOURCE_EXHAUSTED` | a message is larger than `max_message_size` | raise the limit on the side that receives the message |
| Calls fail with `NotGrpc` | the address points at something that is not a gRPC server, or a proxy answered | check the address and the HTTP status in the error |
| `grpcurl` reports a TLS error against a plaintext server | `grpcurl` uses TLS by default | add `-plaintext` |
| `grpcurl` cannot list services | the server does not offer reflection | pass the `.proto` file with `-proto` |
| A TLS client refuses to talk to the server | ALPN does not select `h2` | add `h2` to `alpn_protocols` on both sides |
| A call hangs forever | no timeout and a stalled server | set `timeout()` on every call |

[gRPC]: https://grpc.io
[grpcurl]: https://github.com/fullstorydev/grpcurl
[ntex-grpc]: https://github.com/ntex-rs/ntex-grpc
[`ntex-h2`]: https://docs.rs/ntex-h2
[`NativeType`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/trait.NativeType.html
[`Message`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/trait.Message.html
[`MethodDef`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/trait.MethodDef.html
[`ServiceDef`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/trait.ServiceDef.html
[`Transport`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/client/trait.Transport.html
[`StreamTransport`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/client/trait.StreamTransport.html
[`StreamCall`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/client/struct.StreamCall.html
[`Request`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/client/struct.Request.html
[`RequestContext`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/client/struct.RequestContext.html
[`Response`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/client/struct.Response.html
[`ClientError`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/client/enum.ClientError.html
[`ntex_grpc::client::Client`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/client/struct.Client.html
[`SendStream`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/struct.SendStream.html
[`RecvStream`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/struct.RecvStream.html
[`StreamError`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/enum.StreamError.html
[`GrpcServer`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/server/struct.GrpcServer.html
[`encode_binary_header()`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/fn.encode_binary_header.html
[`encode_grpc_message()`]: https://docs.rs/ntex-grpc/latest/ntex_grpc/fn.encode_grpc_message.html
[examples]: https://github.com/ntex-rs/examples/tree/main/protocols
[helloworld-example]: https://github.com/ntex-rs/examples/tree/main/protocols/grpc-helloworld
[custom-types-example]: https://github.com/ntex-rs/examples/tree/main/protocols/grpc-custom-types
[routing-example]: https://github.com/ntex-rs/examples/tree/main/protocols/grpc-routing-client
[server-example]: https://github.com/ntex-rs/examples/tree/main/protocols/grpc-server
[tls-example]: https://github.com/ntex-rs/examples/tree/main/protocols/grpc-tls
