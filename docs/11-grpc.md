# Building gRPC Services and Clients

Once a system grows past a single process, its parts need a way to call each
other. [gRPC] is one of the most common answers. You describe an API once, in
a `.proto` file, and clients and servers written in any language can talk to
each other through it. Messages are compact Protocol Buffers, calls travel
over HTTP/2, and one connection carries many calls at the same time.

[ntex-grpc] brings gRPC to ntex. It is a separate crate, built on the
[service model](1-service.md) you already know and on [`ntex-h2`], the ntex
HTTP/2 implementation. A gRPC client is a service you call, and a gRPC server
is a service the ntex [server](5-server.md) runs for every connection.

ntex-grpc takes a different route from most gRPC libraries: **it does not use
derive macros or build scripts.** The protocol is described with ordinary
traits, and Rust's type system does the wiring. A message knows how to encode
itself because each of its fields does. A method is a type that knows its
request path and its input and output messages. A client is generic over the
transport that carries its calls. What remains is boilerplate: one struct per
message, one type per method, a typed client per service. A code generator,
`ntex-grpc-codegen`, writes that for you once. You commit its output and read
it like the rest of your code. Nothing is generated during the build. The only
macro you will meet is the `#[server]` attribute, a thin layer that connects
the methods of a `.proto` service to the methods of your type.

Because every piece is a trait, every piece can be replaced. You can use your
own Rust types for message fields, put your own transport between a client
and the network, or handle raw, undecoded calls on the server.

This chapter starts with a running example, then follows a call from the
`.proto` file through the client to the server. Depending on where you are
starting from:

- **New to ntex-grpc?** Run the [quick start](#quick-start-hello-grpc), skim
  [gRPC in a nutshell](#grpc-in-a-nutshell), then read
  [from .proto to Rust](#from-proto-to-rust) and
  [implementing a service](#implementing-a-service).
- **Want stricter types in your messages?** Read
  [using your own field types](#using-your-own-field-types).
- **Calling gRPC services from ntex?** Start with
  [calling services](#calling-services) and
  [errors, deadlines and retries](#errors-deadlines-and-retries).
- **Running a server in production?** Read
  [tuning the server](#tuning-the-server) and
  [running over TLS](#running-over-tls), then go through the
  [checklist](#production-checklist).
- **Something does not work?** See [troubleshooting](#troubleshooting).

The snippets come from the [gRPC examples][examples]. Each example is a
complete program, and its README has the run commands and a `grpcurl`
session to try:

| Example | What it shows |
| --- | --- |
| [grpc-helloworld][helloworld-example] | the smallest service, server and client |
| [grpc-custom-types][custom-types-example] | your own Rust types for message fields |
| [grpc-routing-client][routing-example] | a custom client transport that routes calls to different servers |
| [grpc-server][server-example] | all four kinds of methods, several services, a fallback, server tuning |
| [grpc-tls][tls-example] | gRPC over TLS with rustls |

## Quick start: hello, gRPC

The helloworld example is a greeting service with a single method. Clone the
examples repository and start the server:

```sh
git clone https://github.com/ntex-rs/examples
cd examples/protocols/grpc-helloworld
cargo run --bin grpc-helloworld-server
```

In a second terminal, call it:

```sh
cargo run --bin grpc-helloworld-client -- ntex
```

The client prints `Hello ntex!`. The server speaks standard gRPC, so other
tools can call it too. With [grpcurl] it looks like this:

```sh
grpcurl -plaintext -proto helloworld.proto -d '{"name": "ntex"}' \
    127.0.0.1:50051 helloworld.Greeter/SayHello
```

The whole service is a dozen lines:

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

and the client call is a few more:

```rust,no_run
let client = connect(ADDR);
let msg = HelloRequest { name: "ntex".into() };

let mut req = client.say_hello(&msg);
req.timeout(Duration::from_secs(3));
let reply = req.send().await?;
println!("{}", reply.message);
```

`HelloRequest`, `HelloReply`, the `Greeter` service definition and the
`say_hello` client method all come from the generated file
`src/helloworld.rs`. The rest of the chapter explains where they come from
and how each part works.

To use ntex-grpc in your own project, add these dependencies:

```toml
[dependencies]
ntex = "4"
ntex-grpc = "4"
ntex-h2 = "4"
```

## gRPC in a nutshell

You can use ntex-grpc without knowing the wire protocol. A little of it,
though, explains many of the choices you will make later, especially about
errors and retries.

A gRPC call is an HTTP/2 request:

- It is always a `POST` to `/{package}.{Service}/{Method}`, for example
  `/helloworld.Greeter/SayHello`. The path is how a server finds the method.
- The request headers say that this is gRPC (`content-type:
  application/grpc`) and may carry a deadline (`grpc-timeout`). Your own
  headers, called *metadata* in gRPC, travel next to them.
- Each message in the body has a 5-byte prefix: a flag that says whether it
  is compressed, and its length.
- The response sends headers, the reply messages, and then *trailers*. The
  trailers carry the result of the call in `grpc-status` (`0` means `OK`)
  and an optional `grpc-message`.

The result lives in the trailers, so a failed call still has HTTP status
200. A client knows how a call ended only after it has read the whole
response.

All gRPC methods use the same request. They differ only in how many
messages each side sends:

| Kind | `.proto` | Client sends | Server sends |
| --- | --- | --- | --- |
| unary | `rpc M(Req) returns (Res)` | one message | one message |
| server streaming | `rpc M(Req) returns (stream Res)` | one message | any number |
| client streaming | `rpc M(stream Req) returns (Res)` | any number | one message |
| bidirectional | `rpc M(stream Req) returns (stream Res)` | any number | any number |

Every call is one HTTP/2 stream, so a single connection runs many calls in
parallel, and a slow call does not block the others.

## How ntex-grpc fits together

Here is the path of a call through ntex-grpc:

```text
           .proto ── ntex-grpc-codegen ──▶ src/helloworld.rs
                                        (messages, MethodDefs, typed client)

client:    GreeterClient<T> ─▶ Request ─▶ Transport ─▶ ntex-h2 pool ─▶ network

server:    network ─▶ ntex Server ─▶ GrpcServer ─▶ #[server] type ─▶ your method
```

A handful of traits hold it together:

- [`NativeType`] reads and writes a single field value: an integer, a string,
  a nested message, or a type of your own.
- [`Message`] encodes and decodes a whole message. The generated
  implementation calls `NativeType` for each field.
- [`MethodDef`] describes one `rpc`: its request path and its input and
  output types. Every method is a zero-sized type that implements it.
- [`ServiceDef`] describes a `service`: its full name and its methods.
- [`Transport`] sends a call. A generated client is generic over it, and
  `send()` hands the request to it.

Each of these is a seam you can use. The rest of the chapter goes through
them in the order a call does.

## From .proto to Rust

### Running the code generator

The code generator installs a command named `ntex-grpc`. It uses `protoc`,
the Protocol Buffers compiler, to parse `.proto` files, so install `protoc`
from your package manager first. If it is not in your `PATH`, point the
`PROTOC` environment variable at it.

```sh
cargo install ntex-grpc-codegen
```

Here is the `.proto` file of the helloworld example:

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

You will run the generator every time the `.proto` file changes, so the
examples keep the command in a small `generate.sh` script next to it:

```sh
ntex-grpc helloworld.proto helloworld.rs --out-dir ./src --include-dir ./ \
    --rustfmt-path ../../rustfmt.toml
```

The output file is named after the proto `package`, not after the second
argument. If you generate code for the same package twice, for example with
different options, use two output directories.

| Option | Effect |
| --- | --- |
| `--out-dir DIR` | where to write the generated file |
| `--include-dir DIR` | where to look for imported `.proto` files; can be repeated |
| `--map NAME=TYPE` | use your own Rust type for a field, see [using your own field types](#using-your-own-field-types) |
| `--rustfmt-path FILE` | rustfmt configuration used to format the output |
| `--optional-messages` | generate singular message fields as `Option<T>`, so you can tell an unset field from an empty one |
| `--open-enums` | generate enum fields as `i32`, so values your code does not know yet are kept |
| `--well-known-types` | generate the `google.protobuf` types instead of using `ntex_grpc::google_types` |

Commit the generated file. Reviews then show exactly how a protocol change
affects your Rust code, and your build does not depend on `protoc`.

### What gets generated

Add the generated file as a module, `pub mod helloworld;`. Each `message`
becomes a plain struct:

```rust,no_run
/// The request message with the user's name.
#[derive(Clone, PartialEq, Debug)]
pub struct HelloRequest {
    pub name: ::ntex_grpc::ByteString,
}
```

Each `service` becomes three things:

- a unit struct, `Greeter`, that implements [`ServiceDef`] with the full
  service name, `helloworld.Greeter`;
- a unit struct for each `rpc` that implements [`MethodDef`];
- a typed client, `GreeterClient<T>`, generic over its transport, with one
  method per `rpc`.

The method type is where the type system takes over:

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

`MethodDef` ties a method to its request path and its message types at
compile time. The client uses it to know where to send a request and what to
decode from the reply; the server uses it to decode requests and encode
replies. Passing the wrong message to a method is a compile error, not a
failed call.

Each struct also gets a [`Message`] implementation. It is a list of
`NativeType` calls, one per field, each with the field's tag:

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

There is no reflection and no table of field descriptors at run time, only
straight-line code the compiler can inline. It is also the hook for custom
field types: any type that implements `NativeType` fits into the same code.

### How protobuf types map to Rust

Fields get ntex types where ntex has one, so strings and byte fields share
memory with the received buffer instead of being copied:

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

Here is a message that uses several of them:

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

and the generated Rust:

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

Two defaults are worth knowing about:

- **Enums are closed.** A value that the generated enum does not have, for
  example from a newer version of the `.proto` file, decodes as the enum's
  default variant. If your code must keep such values, generate with
  `--open-enums` and convert with `Color::from_i32()` where you need to.
- **Message fields are always present.** An unset message field decodes as
  the default message, as in proto3. If you need to tell "unset" from
  "empty", generate with `--optional-messages`.

### Using your own field types

Protobuf has few types, and a field often means more than its wire type: a
`bytes` field that always holds a 16-byte UUID, or a `string` that only ever
holds product codes. With generated types, every handler has to check those
rules again. The `--map` option replaces the Rust type of such fields, so
the check happens once, while decoding:

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

The left side of `--map` is a field path. A path with a leading dot is fully
qualified, such as `.inventory.ReserveRequest.sku`. A path without one
matches by suffix: `sku` covers the field in both messages, while
`ReserveRequest.sku` covers only one. The right side is a Rust path, copied
into the generated code as is:

```rust,no_run
pub struct ReserveRequest {
    pub request_id: crate::types::RequestId,
    pub sku: crate::types::Sku,
    pub quantity: u32,
}
```

The wire format does not change, so other gRPC clients and servers still
send plain `bytes` and `string` fields. What changes is the Rust side: the
type decides which values are valid, and decoding fails for anything else.
Your handler never sees a request with a malformed id or SKU, because the
server answers `INVALID_ARGUMENT` before calling it. A `ReserveRequest` in
your code is valid by construction.

### Writing a custom field type

A field type implements [`NativeType`]. The generated structs derive a few
traits, so the type also needs `Default`, `PartialEq` and `Debug`.

| Item | What it does |
| --- | --- |
| `const TYPE: WireType` | the protobuf wire type: `LengthDelimited` for `bytes`, `string` and messages, `Varint` for integers and `bool`, `ThirtyTwoBit` or `SixtyFourBit` for fixed-width numbers |
| `value_len(&self)` | the encoded length of the value, without the tag and length prefix |
| `merge(&mut self, src)` | decode a value; for length-delimited types `src` holds exactly the value, the length prefix is already removed |
| `encode_value(&self, dst)` | write the value, without the tag and length prefix |
| `is_default(&self)` | `true` if the value is the default, so the field is left out of the message |

Here is the request id from the custom-types example. A nil UUID plays the
role of the default value, so it is left out of the message, just like an
empty `bytes` field:

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

You rarely need to handle the wire format yourself. A type that wraps a
built-in one can delegate to it and add its own rules. `Sku` lets
`ByteString` check that the bytes are valid UTF-8, then validates the
string:

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

A few rules keep a custom type compatible with other gRPC implementations:

- **Encode exactly `value_len()` bytes.** For length-delimited types, the
  length prefix is written from `value_len()` before `encode_value()` runs.
  If the two disagree, the message is corrupt.
- **Return an error from `merge`, never panic.** The error becomes
  `INVALID_ARGUMENT` on the server and `InvalidResponse` on the client.
- **Keep `is_default` consistent with protobuf.** Other implementations
  treat a missing field as the protobuf default (empty, zero or `false`), so
  your default must mean the same thing.
- **Override `encoded_len` for types that are not length-delimited.** The
  provided `encoded_len(tag)` adds a length prefix, which is only correct
  for `LengthDelimited`. A `Varint` newtype, such as an id stored as a
  `u64`, must return `key_len(tag) + value_len()` itself, as the built-in
  integer types do; otherwise encoded messages get the wrong size.

The custom-types example tests all of this. It generates the same `.proto`
twice, once with `--map` and once without, and checks that the plain client
and the typed server understand each other.

## Calling services

A generated client has one method per `rpc`. Calling that method does not
send anything yet. It returns a request that you can configure, and then you
send it:

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

The request borrows the client and the message, so nothing is copied until
the message is encoded. Everything you set on it, such as the timeout,
headers or compression, applies to this call only.

### Connecting to a server

The usual transport is [`ntex_grpc::client::Client`]. It is a thin gRPC
layer over an [`ntex-h2`] connection pool:

```rust,no_run
use ntex::SharedCfg;
use ntex_grpc::client::Client;
use ntex_h2::client as h2;

pub fn connect(addr: impl Into<String>) -> GreeterClient<Client> {
    let pool = h2::Client::builder(addr.into()).build(SharedCfg::new("GRPC-CLIENT"));
    GreeterClient::new(Client::new(pool))
}
```

The pool connects lazily. Building it does not touch the network: the first
call opens a connection, and later calls share it, each on its own HTTP/2
stream. When a connection closes, the pool opens a new one for the next call
that needs it. Your code does not have to manage connections at all.

Cloning a `Client` is cheap, and the clones share the pool. Build one
`Client` per server and clone it into every typed client that talks to that
server:

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

HTTP/2 settings, such as window sizes and the ping interval, come from an
`ntex_h2::ServiceConfig` added to the `SharedCfg` that you pass to
`build()`. It works the same way as on the [server](#tuning-the-server).

### What happens during a call

A unary call has three parts:

- **the request**, [`Request`], which holds the transport, the input message
  and a [`RequestContext`] with the headers, the timeout and the limits;
- **the transport**, which turns the request into an HTTP/2 stream;
- **the response**, [`Response`], which holds the decoded reply, the headers
  and the trailers.

`send()` hands the request to the transport through the [`Transport`]
trait:

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

The trait is generic over the method `T`, so a transport knows the method's
`PATH`, `Input` and `Output` at compile time. The built-in transports
(`Client`, `ntex_h2::client::Client` and `ntex_h2::client::SimpleClient`)
send a call in six steps:

1. If a header set on the request was invalid, fail with `InvalidRequest`
   without sending anything.
2. Encode the input message with its `Message` implementation, compress it
   if requested, and add the 5-byte message prefix.
3. Take a connection from the pool and open a stream with `POST T::PATH`,
   the gRPC headers (`content-type: application/grpc`, `te: trailers`, and
   `grpc-timeout` if a timeout is set) and the headers of the request.
4. Send the message and close the sending side of the stream.
5. Read the response headers, the message and the trailers. Check the HTTP
   status, the content type, the message size and `grpc-status`.
6. Decode the message as `T::Output` and return a `Response`.

The timeout covers the whole call, including the wait for a connection.
When it runs out, the stream is reset with `CANCEL` and the call fails with
`DeadlineExceeded`.

A transport can also receive per-call state. `send_with(&state)` passes a
reference to the transport, which reads it with `ctx.request_state()`.
`send()` is simply `send_with(&())`.

### Headers, compression and message limits

Request headers carry metadata such as authentication tokens or trace ids.
Set them on the request before sending it:

```rust,no_run
use ntex_grpc::encode_binary_header;

let mut req = client.say_hello(&msg);
req.header("x-request-id", "42")
    .header("trace-bin", encode_binary_header(&trace_id))
    .timeout(Duration::from_secs(3));

let (reply, headers, trailers) = req.send().await?.into_parts();
```

`header()` replaces a header and `append_header()` adds another value. By
gRPC convention, header names ending in `-bin` carry binary values; encode
them with [`encode_binary_header()`] and decode received ones with
`decode_binary_header()`. The client sets the protocol headers itself
(`content-type`, `te`, `grpc-timeout`, `grpc-encoding` and the others) and
ignores your values for them. An invalid header name or value is not sent;
the call fails with `InvalidRequest` instead.

A successful [`Response`] derefs to the reply message. `headers()` and
`trailers()` give the response metadata, and `into_parts()` takes all three
apart.

**Compression** is optional and needs the `compression` feature:

```toml
ntex-grpc = { version = "4", features = ["compression"] }
```

```rust,no_run
use ntex_grpc::Compression;

req.compression(Compression::Gzip); // or Compression::Zstd
```

Compression costs CPU on both sides, so turn it on for large, repetitive
messages and leave it off for small ones. Empty messages are always sent
uncompressed. A server that does not support the encoding fails the call
with `UNIMPLEMENTED`.

**Message limits** protect the client from huge replies. On `Client`,
`max_message_size()` limits received messages (4 MiB by default), and
`max_send_message_size()` limits sent ones. A request can override both for
a single call.

### Streaming calls

Streaming methods return a `StreamRequest` instead. You configure it the
same way and open it through the [`StreamTransport`] trait. The messages
flow through a [`SendStream`] and a [`RecvStream`]:

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

`join` is `ntex::util::join`. A client-streaming call has to send and wait
for the reply at the same time. If you await `send()` before sending the
messages, the call waits forever: the server answers only after the stream
ends. After a stream ends, `rx.status()` returns its final `grpc-status`.

### Writing your own transport

Generated clients are generic over their transport, so anything that
implements `Transport` (and `StreamTransport`, for streaming methods) can
take the place of `Client`. A transport is the natural home for concerns
that apply to every call: authentication headers, metrics, logging, retries
or routing.

A custom transport usually wraps a built-in one. It adjusts the request
context, then delegates:

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

`GreeterClient::new(WithToken { .. })` now sends the token with every call.
Keep the implementation generic over the request state `St`, so both
`send()` and `send_with()` keep working.

For streaming calls, implement `StreamTransport` the same way. Its `open()`
returns a [`StreamCall`], the HTTP/2 stream of the call, and only the
built-in transports can create one. A custom streaming transport therefore
always ends by calling `open()` on a built-in transport.

Generated clients own their transport. To share one transport between
several typed clients, make it cheap to clone and keep the shared parts
behind an `Rc` or in the pool, as `Client` does.

### Routing calls to different backends

In larger systems, different services often live on different servers. The
routing example puts `shop.Catalog` and `shop.Orders` on separate backends.
A `Router` transport picks the backend for each call from the service name
in `M::PATH`:

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

Both typed clients share one router, and the clones share the routes and
the connection pools. Use the generated `ServiceDef::NAME` constants as
route names. If a service is renamed in the `.proto` file, the code stops
compiling instead of failing at run time:

```rust,no_run
let router = Router::default()
    .route_to(Catalog::NAME, "127.0.0.1:50053")
    .route_to(Orders::NAME, "127.0.0.1:50054");
let catalog = CatalogClient::new(router.clone());
let orders = OrdersClient::new(router);
```

The router returns the same error type as the built-in transports, so
callers handle a missing route like any other failed call.

### Errors, deadlines and retries

Calls fail. Networks drop connections, servers restart, and requests run
into bugs. What matters is how much your code can learn from a failure, and
the most important question is usually: *did the server see my request?*
The answer decides whether a retry is safe.

The built-in transports return `ntex::Error<ClientError>`. It derefs to
[`ClientError`], so match on `&*err`. `err.status()` gives a gRPC status
code for every variant, and `err.message()` describes the cause. Each
variant answers that question:

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

`Status` is the only variant whose status came from the server, and it also
carries the server's trailers. `NotProcessed` covers a connection that was
already closing when the call started and streams that the server refused,
so the request never reached your service.

With that, a retry loop for an idempotent call is short:

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

**Deadlines** follow the gRPC spec. The client sends `timeout()` to the
server in the `grpc-timeout` header, rounded up so the server never gives up
before the client does. When the deadline passes, the server stops the call
and answers `DEADLINE_EXCEEDED`, and the client stops waiting at the same
moment. A zero timeout fails at once, without sending anything.

Without a timeout, a call waits as long as its connection lives. A server
that hangs then holds your call, and whatever is waiting for it, forever.
Set a timeout on every call.

Streaming calls fail in the same ways. `open()` returns the errors of
opening the stream, `recv()` returns a [`StreamError`] if the stream fails
later, and `rx.status()` gives the final status.

## Serving gRPC

A gRPC server has three layers:

- **your type**, with one async method per `rpc`, turned into an ntex
  service by `#[server]`;
- **[`GrpcServer`]**, which speaks the gRPC protocol and routes each call to
  a service by its service name;
- **an ntex [`Server`](5-server.md)**, which accepts connections, optionally
  runs TLS, and hands each connection to `GrpcServer`.

### Implementing a service

Here is the complete helloworld server:

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

`#[server(path)]` takes the generated service definition. Each
`#[method(Name)]` uses the method name from the `.proto` file, and the
attribute checks the argument and return types against the generated
`MethodDef` at compile time. A typo in a method name or a wrong message type
is a compile error. The attribute implements `Service<(), ServerRequest>`
and `NamedService` for your type; everything else is ordinary Rust.

Import the attribute with `use ntex_grpc::server;`. That brings in both the
`server` module and the `server` attribute. `use ntex_grpc::server::{self,
..}` imports only the module, and the confusing error you get is that
`NamedService` is not implemented.

The signature of a method depends on its kind:

| Kind | Arguments | Returns |
| --- | --- | --- |
| unary | `req: Input` or `req: Request<Input>` | `Output`, `Response<Output>`, `Result<Output, E>` with `E: Into<Output>`, or `Result<Response<Output>, E>` with `E: Into<ServerError>` |
| server streaming | `req`, `tx: SendStream<Output>` | `()` or `Result<(), E>` with `E: Into<ServerError>` |
| client streaming | `rx: RecvStream<Input>` | as unary |
| bidirectional | `rx: RecvStream<Input>`, `tx: SendStream<Output>` | as server streaming |

Take `Request<Input>` when you need the request headers. It derefs to the
message, so the fields are still at hand:

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

`GrpcServer` creates a service instance for every connection by calling
`create()`. Where state lives depends on who should see it:

- State for one connection goes into the service that `create()` returns.
- State for one worker can be created in the closure passed to `bind()`,
  which is called for every worker; an `Rc` is enough there. The closure may
  run again when a worker restarts, so keep that setup safe to repeat.
- State for the whole server, such as a counter or a store, goes into an
  `Arc` that all clones share, because workers run on different threads.

### Streaming methods

Streaming methods receive their messages from a `RecvStream` and send them
through a `SendStream`. A server-streaming method gets the request and a
sender:

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

A client-streaming method reads until the client closes its side, then
answers once:

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

A bidirectional method gets both and can answer each message as it arrives:

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

`?` works on failed sends and receives. If the client goes away, the next
`send()` fails and the method returns early. When the method returns, the
call ends and its result becomes the `grpc-status` of the call.

### Reporting errors to clients

Some errors are handled for you, before your method runs:

| Situation | Status |
| --- | --- |
| unknown service or method | `UNIMPLEMENTED` |
| request message that cannot be decoded, including a custom field type that rejects its value | `INVALID_ARGUMENT` |
| request message larger than `max_message_size` | `RESOURCE_EXHAUSTED` |
| compressed request in an encoding the server does not support | `UNIMPLEMENTED` |
| deadline from `grpc-timeout` runs out | `DEADLINE_EXCEEDED` |

Your methods report their own errors through their return value. A
`ServerError` becomes the `grpc-status` and `grpc-message` of the call, and
its optional `HeaderMap` adds more trailers. Build it with
`ServerError::new()`, and pass the text through [`encode_grpc_message()`],
because gRPC expects `grpc-message` to be percent-encoded.

Streaming methods return `Result<(), ServerError>`, as shown above. Unary
and client-streaming methods return a reply, so they need a way to say
whether the `Err` side is a failure. Return `Result<Response<Output>,
ServerError>` to fail the call with a status. `Response` also carries
extra trailers for a successful reply, and a plain message turns into one
with `.into()`:

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

The trailers in `Response::headers` follow `grpc-status: 0`. A
`grpc-status` or `grpc-message` among them is ignored, so a `Response` is
always a successful call. The client reads them from `trailers()` on its
response.

A method that returns `Result<Output, E>` works differently: it **always
ends with `OK`**. Its error type is converted into a reply message
(`E: Into<Output>`), so the error has to be part of the reply. That suits
APIs that model errors in the protocol anyway, where a `oneof` gives the
reply an explicit success or failure:

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

Errors in the reply are part of your API: clients get a typed `Problem`
instead of a status code and a string. Pick a gRPC status when the failure
is generic, such as a missing resource or a denied permission, and a
`oneof` when clients need to act on the details.

### Hosting several services

One `GrpcServer` can serve several services, and it routes calls by the
service name in the request path:

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

Without a default service, a call to an unknown service fails with
`UNIMPLEMENTED`. Adding the same name twice panics, so the mistake shows up
when the server starts, not when the first call arrives.

The default service and `service_named` services are plain ntex services of
`ServerRequest`, written by hand. They see the call before anything is
decoded: the service and method names, the request headers and the encoded
payload. They answer with a `ServerResponse` holding the encoded reply, or
with a `ServerError` holding any status. They handle only unary calls.

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

`Fallback` also needs a `ServiceFactory` implementation, just like the
`#[server]` types.

### Tuning the server

`GrpcServer` has two gRPC limits:

- `max_message_size(size)` is the largest request message the server
  accepts, checked again after decompression. A larger one fails the call
  with `RESOURCE_EXHAUSTED`. The default is 4 MiB.
- `max_send_message_size(size)` is the largest reply message the server
  sends.

Everything below gRPC is HTTP/2. `GrpcServer` reads its HTTP/2 settings
from the configuration of the listener it is bound to, so add an
`ntex_h2::ServiceConfig` to the `SharedCfg` passed to `bind()`:

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

The defaults suit most services. Change them when your traffic is unusual:

| Setting | Default | Why change it |
| --- | --- | --- |
| `set_max_concurrent_streams` | 256 | the number of calls a client can run in parallel on one connection |
| `set_initial_window_size` | 65,535 | larger windows speed up large messages and streams |
| `set_initial_connection_window_size` | 4 MiB | the receive window shared by all calls on a connection |
| `set_ping_timeout` | 10 seconds | detect dead connections; zero disables pings |
| `set_handshake_timeout` | 5 seconds | how long a new connection has to complete the HTTP/2 handshake |

With the `compression` feature, the server also accepts gzip and zstd
requests and compresses each reply like its request, unless the client's
`grpc-accept-encoding` header rules that out.

### Running over TLS

gRPC needs HTTP/2, and `GrpcServer` speaks only HTTP/2. It serves any
[`Io`](7-io.md), so TLS is simply one more service in front of it. On the
server, the TLS acceptor runs first and passes the decrypted stream on:

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

On the client, give the pool a TLS connector and turn on `secure(true)`, so
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

Both sides must offer `h2` in ALPN (`config.alpn_protocols =
vec![b"h2".to_vec()]`); many gRPC clients refuse a server that does not
select it. The client must connect to a host name that matches the server
certificate, such as `localhost:50056`. Certificates, trust roots and the
other TLS backends are covered in the [TLS chapter](10-tls.md).

Without TLS, ntex-grpc uses HTTP/2 with *prior knowledge*: the client
starts with the HTTP/2 preface right away, with no HTTP/1.1 upgrade. That is
what gRPC clients expect for plaintext connections. Tools such as `grpcurl`
need the `-plaintext` flag to talk to such a server.

## Testing your services

Clients and servers are ordinary ntex services, so tests use the ntex test
server. Start the gRPC server on a random port, connect a generated client
to it, and make real calls:

```rust,no_run
use ntex::server::TestServerBuilder;

/// STATEMENT: A unary handler that takes `Request<_>` reads the request headers.
///
/// HOW:
/// - Call `UnaryEcho` with the `x-echo-prefix` header set to `> `.
/// - Check the reply is the message with the prefix in front.
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

`TestServerBuilder::config()` takes the same `SharedCfg` as the binary, so
tests run with the production HTTP/2 settings. A few more patterns from the
examples:

- **Test unknown methods.** Implement `MethodDef` by hand with a `PATH` the
  server does not have, send it with `ntex_grpc::client::Request::new`, and
  check the `UNIMPLEMENTED` or fallback answer.
- **Test wire compatibility.** Generate the same `.proto` twice, once with
  `--map` and once without, and call the server that uses custom types with
  the plain client. That is exactly what other implementations see.
- **Unit-test custom field types.** Encode a message with
  `Message::write`, decode it with `Message::read`, compare the two, and
  check that `merge` rejects invalid input with an error.
- **Set a timeout on every call in tests**, so a broken server fails the
  test instead of hanging it.

## Troubleshooting

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

## Production checklist

- **The generated code is up to date.** `generate.sh` lives next to the
  `.proto` files and its output is committed, so reviews show protocol
  changes.
- **Custom field types match the wire.** `value_len` is exact, `is_default`
  matches the protobuf default, `merge` returns errors instead of panicking,
  and varint types override `encoded_len`.
- **Every call has a deadline.** Clients set `timeout()`, so a stalled
  server cannot hold a call forever.
- **Retries look at the error.** Only `NotProcessed`, `Connection` with
  `request_sent == false`, or failures of idempotent methods are retried.
- **Clients share pools.** There is one `Client` per backend, cloned into
  the typed clients and transports that use it.
- **Shared state is in an `Arc`.** Services are created per connection, on
  several worker threads.
- **Errors reach clients with the right status.** Methods return
  `ServerError`s for failed calls, or model errors in the reply with a
  `oneof`.
- **Limits are set.** `max_message_size` fits the largest expected message
  on both sides, and the HTTP/2 stream and window settings fit the load.
- **Compression is a deliberate choice.** If clients compress, the server
  is built with the `compression` feature.
- **TLS offers `h2`.** Both sides use ALPN `h2`, and clients use
  `secure(true)` and a host name from the certificate.

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
