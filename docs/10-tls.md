# Securing Connections with TLS

Sooner or later, almost every service has to leave `localhost`. When it does,
its traffic crosses networks you do not control, and you want two guarantees:
nobody in between can read or alter the data, and each side is talking to the
peer it expects. TLS provides both.

The good news is that adding TLS to an ntex application changes how you set up
a connection, not how you handle its messages. TLS encrypts the traffic and
lets peers authenticate each other, while your HTTP handlers, WebSocket
services, and custom protocols continue to work with ordinary application
bytes.

TLS does not depend on the runtime's I/O implementation either. All ntex TLS
backends work on in-memory byte streams through the I/O filter layer; they do
not read from or write to sockets themselves. You can use them with any
ntex-supported I/O or runtime backend, as long as the TLS backend supports your
platform.

ntex also provides a platform-independent way to work with X.509 certificates
and identities through its [`x509`] module. You use a common API for loading
identities, inspecting certificates, and checking trust, while ntex uses the
appropriate platform or backend functionality underneath.

This chapter starts with a small HTTPS server you can run in a few minutes. It
then walks through choosing a backend, configuring connections, and working
with certificates. Depending on where you are starting from:

- **New to TLS in ntex?** Read the [quick start](#quick-start-an-https-server)
  and the [certificate primer](#certificates-in-a-nutshell).
- **Already have a TLS configuration?** Go straight to
  [configuring ntex components](#configuring-ntex-components).
- **Need to identify your clients?** Start with
  [client-certificate authentication](#client-certificate-authentication).
- **Preparing for production?** Finish with the
  [checklist](#production-checklist).

The snippets focus on configuration. For complete programs, use the
[ntex examples repository][examples]; each example's README includes dependencies,
certificate setup, and run commands.

## Supported backends

ntex supports three TLS backends. Their types are available through `ntex::tls`,
or directly from `ntex_tls`.

| Backend | Enable it | Platform and configuration |
| --- | --- | --- |
| OpenSSL | `ntex/openssl` or `ntex-tls/openssl` | Platforms with OpenSSL; requires its native libraries and headers |
| rustls | `ntex/rustls` or `ntex-tls/rustls` | Configured with rustls `ClientConfig` and `ServerConfig`; choose a crypto provider |
| Schannel | Available automatically on Windows | Windows-native identities, trust policy, protocol versions and cipher suites |

Choose OpenSSL or rustls by enabling the corresponding feature; neither is
enabled by default in `ntex-tls`. Schannel is already available on Windows, so
you do not need to enable its compatibility-only `schannel` feature.

For an OpenSSL application, start with:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio", "openssl"] }
openssl = "0.10"
```

For rustls, you also need a crypto provider: the implementation of the
cryptographic algorithms. The examples in this chapter use ring:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio", "rustls"] }
ntex-tls = { version = "4.7", features = ["rustls-ring"] }
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
rustls-pemfile = "2"
```

Here, `ntex-tls/rustls-ring` enables rustls and makes ring available to the
X.509 helpers when no process-wide provider has been installed.

Both examples choose Tokio, but you can use another ntex-supported runtime
without changing the TLS configuration.

### Choosing a backend

All three backends give you the same ntex integration, so the choice is mostly
about your platform and operations rather than your application code:

- **rustls** is a good default for new services. It is written in Rust, needs
  no system TLS library, and makes cross-compilation and static builds simple.
  You choose its crypto provider explicitly.
- **OpenSSL** fits best when your organization already standardizes on it, when
  you need to reuse existing OpenSSL configuration, or when you need a feature
  only OpenSSL provides here, such as pre-shared-key (PSK) identities.
- **Schannel** is the natural choice for Windows-first deployments. It uses the
  Windows certificate stores, trust policy, and keys that can be used without
  being exported.

Because protocol code does not depend on the backend, switching later usually
means changing only the configuration code shown in this chapter.

### TLS and X.509 backend selection are different

The backend that encrypts a connection is not necessarily the one that loads
or verifies certificates. You can enable both TLS features and choose a
transport for each connection, but the [`x509`] helpers follow a separate rule:
they always use native APIs on Windows; elsewhere, they use rustls when enabled
and OpenSSL otherwise. If verification fails, they do not retry with another
backend.

The `x509` module is always available on Windows. On other platforms, enable
OpenSSL or rustls; enabling `x509` alone is not enough.

There is one extra dependency to keep in mind: on Unix, loading a PKCS#12 archive
requires OpenSSL, even if rustls handles your TLS connections and certificate
verification.

## Quick start: an HTTPS server

Before looking at the moving parts, here is the whole thing working. This
program serves one route over HTTPS using rustls and the dependencies listed
above:

```rust,no_run
use std::sync::Arc;

use ntex::tls::{rustls::certified_key, x509::Identity};
use ntex::web::{self, App, HttpResponse};
use rustls::{ServerConfig, sign::SingleCertAndKey};

#[ntex::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The server's certificate chain and private key.
    let identity = Identity::from_pem(
        &std::fs::read("certs/server.pem")?,
        &std::fs::read("certs/server.key")?,
    )?;
    let key = certified_key(&identity)?;

    let config = ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(SingleCertAndKey::from(key)));

    web::server(async |_| {
        App::new().service(
            web::resource("/").to(|| async { HttpResponse::Ok().body("Hello over TLS!") }),
        )
    })
    .bind_rustls("127.0.0.1:8443", &config, ntex::SharedCfg::new("HTTPS"))?
    .run()
    .await?;
    Ok(())
}
```

You need a certificate to run it. The quickest way is to clone the
[examples repository][examples], run `sh tls/generate-certs.sh` from its root,
and copy `server.pem`, `server.key`, and `ca.pem` from `tls/certs` into a
`certs` directory next to your project. Then, in another terminal:

```sh
curl --cacert certs/ca.pem https://localhost:8443/
```

The `--cacert` option tells curl to trust the example certificate authority.
Without it, curl rightly refuses the connection because nobody it trusts has
vouched for the server.

Notice what did *not* change: the application, its routes, and its handler are
the same as for a plain HTTP server. Only the setup code loads an identity,
builds a rustls `ServerConfig`, and calls `bind_rustls` instead of `bind`. The
rest of this chapter explains each of those pieces, and how to do the same with
the other backends and on the client side. The [rustls example][rustls-example]
is a fuller version of this program.

## TLS as an I/O filter

Think of TLS as another layer between the socket and your protocol. On reads,
it decrypts bytes before your application sees them. On writes, it encrypts
the bytes your application produces:

```text
socket <-> TLS <-> HTTP
socket <-> TLS <-> WebSocket <-> application protocol
```

Reads move from left to right in these diagrams; writes move in the opposite
direction. The filters transform buffers, while the I/O backend handles the
socket. The [I/O filter subsystem](7-io.md#filter-subsystem) chapter explains
this model in more detail.

**Client and server protocol code does not need to depend on a TLS backend to
operate over TLS.** Write your protocol against ntex I/O rather than a bare
TCP socket, and leave the transport choice to the connection setup:

| Connection stage | Responsibility |
| --- | --- |
| Server listener | Accept a transport connection |
| TLS acceptor | Upgrade existing `Io` through a server handshake |
| TLS connector | Establish a transport and perform a client handshake |
| Protocol service or codec | Process application bytes, independent of the installed TLS backend |

Adding TLS preserves any filters already installed on the connection. The type
`Io<Layer<TlsFilter, F>>` describes that stack. Your protocol can stay generic
over `F: Filter`, or use `Io::seal` or `Io::boxed` to hide the concrete filter
type. Hiding the type does not change the connection: encryption, metadata,
readiness, and shutdown behavior all remain in place.

For example, this byte-stream service has no TLS imports:

```rust,no_run
use ntex::codec::BytesCodec;
use ntex::io::IoBoxed;

async fn echo(io: IoBoxed) -> std::io::Result<()> {
    while let Some(bytes) = io.recv(&BytesCodec).await? {
        io.send(bytes, &BytesCodec).await?;
    }
    Ok(())
}
```

The same function accepts plain I/O or `io.boxed()` after a TLS handshake.
It echoes byte chunks, not complete application messages: if your protocol
has message boundaries, use a framing codec instead of `BytesCodec`.

You can see this separation in the examples. The
[connection-state example][state-example] performs a handshake after accepting
a connection and before constructing its HTTP state. The
[WebSocket example][ws-example] uses the same frame handlers for plain and
encrypted connections.

There is one important exception: if your application uses certificates to
decide who may connect, that part of the application needs to inspect and verify
the peer's certificate.

## Certificates in a nutshell

Most TLS problems are really certificate problems, so a little vocabulary goes
a long way. If you already know how certificate chains work, skip ahead.

A **certificate** is a signed statement: "this public key belongs to these
names." The names live in the certificate's subject alternative names (SANs),
for example `DNS:api.example.com`, `IP:10.0.0.5`, or a URI such as
`spiffe://example.org/billing`. The certificate also carries a validity period
and the identity of its **issuer**, the party that signed it.

The matching **private key** never leaves its owner. During the handshake, a
peer proves that it holds the key, so a stolen copy of a certificate alone is
useless. In ntex, a certificate chain together with its private key is called an
**identity**.

Issuers are themselves identified by certificates, which forms a **chain**:

```text
leaf (api.example.com)  ->  intermediate CA  ->  root CA
```

The leaf identifies the endpoint. Intermediates connect it to a root. A root is
self-signed, so it proves nothing by itself; it is useful only because *you*
decided to trust it. Those locally configured certificates are called **trust
anchors**, and certificate authorities (CAs) are the organizations or internal
services that issue certificates.

To verify a peer, a TLS implementation checks that:

1. the peer proves it owns the leaf's private key;
2. the chain leads from the leaf to one of your trust anchors;
3. every certificate in the path is within its validity period and allowed for
   this use;
4. for a server, the leaf's names include the host the client meant to reach.

Nearly every failure you will see corresponds to one of these steps: an unknown
issuer, an expired certificate, or a name mismatch. Keep the list in mind when
reading error messages, and fix the configuration rather than disabling the
check.

## Configuring a backend

A TLS configuration answers four questions:

- Which identity, meaning a certificate chain and private key, do I present?
- Which certificate authorities do I trust to identify the other side?
- As a client, which server name do I expect?
- Which application protocols can we use? TLS negotiates these through
  Application-Layer Protocol Negotiation (ALPN).

The server name deserves particular care. The connector normally checks the
host from the request, not just the address it connects to. A certificate for
`localhost` does not also identify `127.0.0.1` unless it lists that IP address.
Server Name Indication (SNI) helps the server choose which certificate to
present; it does not replace the client's verification.

To try the examples, first run [`tls/generate-certs.sh`][cert-setup] from the
examples repository root:

```sh
sh tls/generate-certs.sh
```

The script creates a local certificate authority (CA) and certificates with
subject alternative names (SANs), which identify their hosts or application
identities. It also creates unencrypted PKCS#8 private keys. These files are
gitignored and meant only for local development; never deploy them.

Run each program from its own example directory so the relative certificate
paths resolve correctly. If a connection fails, check the trust configuration
and certificate names rather than turning off verification.

### OpenSSL

The [OpenSSL example][openssl-example] includes an HTTPS server and client,
plus a mutual TLS (mTLS) server that authenticates clients.

Start the client configuration by enabling verification and loading the CA
certificates it should trust:

```rust,no_run
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

fn client_config() -> Result<SslConnector, openssl::error::ErrorStack> {
    let mut config = SslConnector::builder(SslMethod::tls())?;
    config.set_verify(SslVerifyMode::PEER);
    config.set_ca_file("../certs/ca.pem")?;
    config.set_alpn_protos(b"\x02h2\x08http/1.1")?;
    Ok(config.build())
}
```

The ntex connector also checks the server name against the connection host.
For production, replace the example CA with the appropriate system or
organization trust roots.

The server needs an identity to present to clients. Load it and add it to the
acceptor configuration:

```rust,no_run
use ntex::tls::{openssl::set_identity, x509::Identity};
use openssl::ssl::{SslAcceptor, SslAcceptorBuilder, SslMethod};

fn server_config() -> Result<SslAcceptorBuilder, Box<dyn std::error::Error>> {
    let identity = Identity::from_pem(
        &std::fs::read("../certs/server.pem")?,
        &std::fs::read("../certs/server.key")?,
    )?;
    let mut config = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())?;
    set_identity(&mut config, &identity)?;
    Ok(config)
}
```

`set_identity` installs the private key, the endpoint's certificate (the leaf),
and its intermediate certificates in chain order. It leaves out the self-signed
root and does not change how peers are verified. The
`mozilla_intermediate_v5` preset supports modern TLS, including TLS 1.3.

You can pass these configurations to the HTTP helpers below, or build
`ntex::tls::openssl::SslConnector` and `SslAcceptor` services directly. If you
are building a custom protocol, set ALPN for that protocol instead of copying
the HTTP protocol list.

### rustls

The [rustls example][rustls-example] selects ring explicitly for TLS and enables
`rustls-ring` for the portable identity and trust helpers. Loading or converting
an identity does not silently install a process-wide crypto provider.

A client needs both a crypto provider and a root store:

```rust,no_run
use std::{fs::File, io::BufReader, sync::Arc};
use rustls::{ClientConfig, RootCertStore};

fn client_config() -> Result<ClientConfig, Box<dyn std::error::Error>> {
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut BufReader::new(File::open("../certs/ca.pem")?)) {
        roots.add(cert?)?;
    }
    let mut config = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}
```

Here, `with_no_client_auth` means "do not send a client certificate."
The client **still verifies the server**.

On the server side, convert the portable identity into a rustls signing key.
A certificate resolver then makes that identity available during the handshake:

```rust,no_run
use std::sync::Arc;
use ntex::tls::{rustls::certified_key, x509::Identity};
use rustls::{ServerConfig, sign::SingleCertAndKey};

fn server_config() -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let identity = Identity::from_pem(
        &std::fs::read("../certs/server.pem")?,
        &std::fs::read("../certs/server.key")?,
    )?;
    let key = certified_key(&identity)?;
    let mut config = ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(SingleCertAndKey::from(key)));
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}
```

On this side of the connection, `with_no_client_auth` means "do not ask for a
client certificate." The server still presents its own identity. To require
client certificates, configure a client verifier instead.

Be careful if your dependencies enable more than one rustls crypto provider.
Before using a default builder that needs a provider, explicitly install the
one you intend to use process-wide. `builder_with_provider` only configures
that builder; it does not select a provider for every other builder or X.509
operation.

### Windows Schannel

The [Schannel example][schannel-example] uses Windows' native TLS backend.
Its default client configuration verifies server certificates and names using
Windows trust policy:

```rust,no_run
#[cfg(windows)]
fn client_config() -> ntex::tls::schannel::ClientConfig {
    ntex::tls::schannel::ClientConfig::new()
}
```

This client offers `h2` and `http/1.1`, but sends no client certificate.
To send one, call `set_client_cert` explicitly; Schannel does not choose an
identity from the user's certificate stores for you.

A server requires a native identity:

```rust,no_run
#[cfg(windows)]
fn server_config(
    identity: &ntex::tls::x509::Identity,
) -> std::io::Result<ntex::tls::schannel::ServerConfig> {
    ntex::tls::schannel::ServerConfig::new(identity.schannel().clone())
}
```

By default, the server does not ask for client certificates. You can change
ALPN with `set_alpn_protocols` on either configuration. The available TLS
versions and cipher suites depend on Windows policy and OS support.

This client configuration does not provide a separate root store for verifying
servers. For local development, follow the example README to add and later
remove the example CA in an isolated Windows account. Keep verification enabled;
configure Windows trust rather than bypassing it.

## Configuring ntex components

Once you have a backend configuration, attach it to the client or server that
opens the connection. The APIs differ slightly between HTTP and WebSocket
clients, so use the section that matches your application.

### HTTP client

For HTTPS, pass your configuration to `ClientBuilder::openssl(config)` or
`ClientBuilder::rustls(config)`. Both install a secure connector, and the rest
of the client works as usual:

```rust,no_run
async fn fetch(config: rustls::ClientConfig) -> Result<(), Box<dyn std::error::Error>> {
    let client = ntex::client::Client::builder()
        .rustls(config)
        .build(ntex::SharedCfg::new("HTTPS"));
    let response = client.get("https://localhost:8443/").send().await?;
    println!("{}: {}", response.status(), String::from_utf8_lossy(&response.body().await?));
    Ok(())
}
```

Build the client once and reuse it: it keeps a connection pool, so later
requests to the same host can skip the TLS handshake.

If you have a custom TLS connector service, including Schannel, pass it to
`secure_connector`:

```rust,no_run
#[cfg(windows)]
fn client() -> ntex::client::Client {
    use ntex::tls::schannel::{ClientConfig, TlsConnector};
    ntex::client::Client::builder()
        .secure_connector(TlsConnector::with_config(ClientConfig::new()))
        .build(ntex::SharedCfg::new("HTTPS"))
}
```

Do not confuse this with `ClientBuilder::connector`: that method configures
**unsecured** connections and leaves the HTTPS connector unchanged.

The built-in default OpenSSL connector verifies server certificates against
OpenSSL's default trust store (the system roots) and checks the request host.
To trust a private root, supply your own configuration, such as the OpenSSL
configuration above.

The default rustls configuration uses webpki roots and sends no client identity.
If you enable both backend features, the default HTTP client selects OpenSSL.
On Windows without either feature, install Schannel explicitly through
`secure_connector`.

The [HTTP client example][http-client-example] shows a custom HTTPS connector.
The backend examples also show how to use private trust roots and client
identities. Once the client is built, request and body handling stay the same.
When wrapping connection errors, keep the original error in an application
error type or `Box<dyn std::error::Error>` rather than turning it into an
`io::Error` containing only a string.

### WebSocket client

`WsClient` has its own connector and does not inherit settings from an HTTP
`Client`. Its default connector uses plain TCP: **a `wss://` URL alone does
not enable TLS.**

Install a TLS connector explicitly. These WebSocket APIs use an HTTP/1.1
upgrade, not HTTP/2 extended CONNECT, so offer `http/1.1` rather than `h2`:

```rust,no_run
use ntex::{SharedCfg, ws::WsClient};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

#[ntex::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = SslConnector::builder(SslMethod::tls())?;
    config.set_verify(SslVerifyMode::PEER);
    config.set_ca_file("../../tls/certs/ca.pem")?;
    config.set_alpn_protos(b"\x08http/1.1")?;
    let connection = WsClient::new("wss://localhost:8443/ws/", SharedCfg::default())
        .openssl(config.build())
        .connect()
        .await?
        .seal();
    println!("Upgrade response: {:?}", connection.response());
    Ok(())
}
```

With rustls, set `config.alpn_protocols = vec![b"http/1.1".to_vec()]` and use
`.rustls(Arc::new(config))`. With Schannel, restrict ALPN in `ClientConfig` and
use `.connector(TlsConnector::with_config(config))`.

Unlike the HTTP client's `connector`, `WsClient::connector` replaces the whole
connector for this connection, including its TLS behavior.

The [secure WebSocket example][ws-example] calls `seal` for both plain and TLS
connections. That lets both paths use the same message-handling loop.

### HTTP and WebSocket servers

For web applications, use the backend-specific HTTP server helpers:

| Method | Arguments after `self` |
| --- | --- |
| `bind_openssl` | address, `SslAcceptorBuilder`, shared configuration |
| `bind_rustls` | address, `&ServerConfig`, shared configuration |
| `listen_openssl` | existing listener, shared configuration, `SslAcceptorBuilder` |
| `listen_rustls` | existing listener, shared configuration, `ServerConfig` |

These helpers perform the TLS handshake before serving HTTP and configure
HTTP ALPN for you. Keep your existing routes, middleware, and handlers.

A WebSocket connection starts as HTTP, so the server setup is the same: use
an HTTPS listener and keep your upgrade and frame handlers. TLS stays in place
after the upgrade; `web::ws::start` does not remove it.

Use the lower-level server builder when you need Schannel, a custom protocol,
or a certificate check before serving any requests. Compose the services in
the order you want them to run:

```text
listener -> TLS acceptor -> optional authorization gate -> HttpService / protocol service
```

There is no `bind_schannel` web-server helper. The
[Schannel server][schannel-example] and both mutual-authentication examples
show this service chain. Their HTTP service does not depend on the concrete
TLS filter, and they use a shared boxed error type to preserve errors from
the handshake, authentication, and HTTP layers.

### Shared handshake configuration

Use [`TlsConfig`] to control how long a TLS handshake may take. Attach it to
the server's accepted I/O configuration or the client connector's `SharedCfg`:

```rust
use ntex::{SharedCfg, time::Seconds, tls::TlsConfig};

let cfg: SharedCfg = SharedCfg::new("TLS")
    .add(TlsConfig::new().set_handshake_timeout(Seconds(10)))
    .into();
```

Pass `cfg` to the server's bind/listen call, `ClientBuilder::build`, or
`WsClient::new`. The default TLS handshake timeout is five seconds; zero
disables the deadline.

This timeout covers only TLS negotiation. DNS lookup, establishing a TCP
connection, waiting for an HTTP response, and the WebSocket opening handshake
have separate settings in their respective components.

## X.509 certificates and identity loading

The [`x509`] module provides a platform- and backend-independent API for
working with X.509 certificates and identities. It uses platform- and
backend-specific functionality underneath, so your application can use common
types instead of calling each backend directly. The API is portable, but
supported features and verification behavior can still differ by backend.

Before loading certificate files, it helps to separate three concepts:

| Type | Meaning |
| --- | --- |
| `Certificate` | A parsed public certificate, without a private key |
| `Identity` / `ClientIdentity` | Certificate chain plus a matching private key |
| `CaCerts` | Certificates you explicitly trust when verifying clients |

Loading an identity checks that its data is well-formed and the key matches
the certificate. It does not tell you whether to trust that identity, grant it
access, or accept it for a particular hostname. Parsing a certificate likewise
reads its contents; it does not authenticate whoever sent it.

The [X.509 example][x509-example] includes an inspector you can run against your
files, plus the authentication helpers shared by all three backend examples.

### PEM and DER

PEM is a text encoding; DER is binary. To load an identity from PEM, use
`Identity::from_pem(certificates, private_key)` with the leaf certificate first
in the chain and an **unencrypted PKCS#8** private key.
`Identity::from_der(&[&[u8]], private_key)` takes the equivalent DER inputs.
If your key is in PKCS#1, SEC1, or encrypted PEM form, convert it before using
these portable loaders.

`Certificate::from_pem` reads a certificate; `from_pem_multi` reads a bundle.
`Certificate::from_der` parses owned DER bytes. These operations need no private
key.

If you are loading from files, `IdentitySource` handles the file reads for you:

```rust,no_run
use ntex::tls::x509::{Identity, IdentitySource};

fn load() -> Result<Identity, ntex::tls::x509::Error> {
    IdentitySource::PemFiles {
        certificates: "../certs/server.pem".into(),
        private_key: "../certs/server.key".into(),
    }
    .load()
}
```

In the examples, `load_identity("../certs/server")` is a convenience helper
that loads `server.pem` and `server.key`.

### PKCS#12 and Windows stores

PKCS#12 archives bundle certificates and private keys. If the archive has a
password, use `Identity::from_pkcs12(bytes, password)`;
`IdentitySource::Pkcs12File(path)` uses an empty password.

Be explicit about the format: parsing a plain string as `IdentitySource`
treats it as a PKCS#12 path, **not** a PEM path. Load passwords from a secret
source or protected environment configuration, not command-line arguments,
and keep them out of logs.

PKCS#12 loading uses native APIs on Windows and requires the `openssl` feature
on Unix. You can still use rustls for TLS; using OpenSSL to import an archive
does not choose the connection's TLS backend.

On Windows, you can also select an identity and its key from a certificate store:

```text
cert:CurrentUser\MY\<40-hex-SHA1-thumbprint>
cert:LocalMachine\MY\subject=<name>
```

A thumbprint selector does not check whether the certificate is currently
valid. A subject selector picks a currently valid match with an accessible
private key, choosing the one with the latest expiry. Neither verifies trust.
These selectors work only on Windows.

Windows keys may be non-exportable: a backend can use the key to sign without
being able to read its bytes. Key lifetime also matters when importing an
identity. `Identity` keeps an isolated imported key for Schannel until its last
owner is dropped, then normally deletes it. An abnormal process exit can leave
that key behind. `ClientIdentity` imports use ephemeral native keys instead,
which is useful when the identity is only needed by a client.

### Using an identity with each backend

| Backend | Adapter | Important limitation |
| --- | --- | --- |
| OpenSSL | `ntex::tls::openssl::set_identity(&mut context, &identity)` | Requires an exportable PKCS#8 key; cannot use opaque Windows CNG keys through this adapter |
| rustls | `certified_key(&identity)` or `client_certified_key(&client_identity)` | Install the result through a certificate resolver; native Windows keys can sign without being exported |
| Schannel | `identity.schannel().clone()` | Windows-only native identity for client/server configuration |

For rustls, use `SingleCertAndKey` when you have one configured identity; it
works for both clients and servers. `ClientIdentity::from_identity` prepares
an ordered client chain from an existing identity. You can also load a
`ClientIdentity` directly from PEM, DER, or PKCS#12.

The OpenSSL and rustls adapters put the chain in order for you. Underneath,
`Identity::ordered_chain(false)` arranges the intermediates, leaves out the
self-signed root, and drops unrelated certificates with a warning.

Sending a root certificate does not make the other side trust it. Trusted
certificates belong in the verifier's configuration, not in a chain supplied
by the peer.

## Client-certificate authentication

With ordinary HTTPS, the client authenticates the server. Mutual TLS also asks
the client to present a certificate and prove that it owns the corresponding
private key. Your application still needs to answer three separate questions:

1. Did the client provide a certificate?
2. Is that certificate trusted?
3. Is that identity allowed to use this application?

Start by configuring the server to ask for or require a client certificate:

| Backend | Requiring or requesting a client identity |
| --- | --- |
| OpenSSL | Set `PEER \| FAIL_IF_NO_PEER_CERT` and configure client trust anchors |
| rustls | Configure `WebPkiClientVerifier` with client roots; do not enable anonymous clients when a certificate is mandatory |
| Schannel | `request_client_cert(true)` requests an optional certificate and proof of key possession; the application must require and verify it |

In OpenSSL, `PEER` alone does not require the client to send a certificate.
In rustls, making the verifier optional is not the same as turning off client
authentication altogether.

**Schannel only requests a certificate; it does not enforce your authentication
policy.** The handshake can succeed without a certificate, and it does not
check the client's certificate trust here. Your application must reject missing
or unacceptable identities before doing any protected work. The example makes
this check before `HttpService`, so rejection closes the connection without
sending an HTTP response.

The client also needs its own identity configured: use `set_identity` with
OpenSSL, a certificate resolver with rustls, or `set_client_cert` with Schannel.
This is separate from the roots it uses to trust the server. Sending a client
certificate does not replace checking the server's name and certificate chain.

To try the OpenSSL example, run these from `tls/openssl` in separate terminals:

```sh
cargo run --bin openssl-mtls-server
cargo run --bin openssl-client -- https://localhost:8443/ ../certs/client
```

With the expected client identity, the response is `authorized`. Try again
without an identity, with `untrusted-client`, and with `other-client`; each
connection should be rejected. The last case is worth noticing:
`other-client` has a valid certificate from a trusted issuer, but not the
identity this application allows.

The rustls and Schannel READMEs provide equivalent commands.

## Reading peer certificates

When your application needs the peer's certificate, use [`PeerCertDer`] and
[`PeerCertChainDer`]. These queries work across backends, unlike native
certificate-chain queries whose conventions differ.

This helper works with `IoRef` from any supported TLS backend:

```rust
use ntex::io::IoRef;
use ntex::tls::{PeerCertChainDer, PeerCertDer};
use ntex::tls::x509::{self, PeerCerts};

fn peer_certs(io: &IoRef) -> Result<Option<PeerCerts>, x509::Error> {
    let Some(leaf) = io.query::<PeerCertDer>().as_ref().cloned() else {
        return Ok(None);
    };
    let chain = io.query::<PeerCertChainDer>()
        .as_ref()
        .cloned()
        .map(|item| item.0)
        .unwrap_or_default();
    PeerCerts::from_der(leaf.0, chain).map(Some)
}
```

The query values own their DER buffers. `PeerCerts::from_der` parses them and
removes a duplicated leaf if it appears at the start of the supplied chain.
`PeerCerts::chain` holds the remaining chain, which may be incomplete or
untrusted. On versions that provide `QueryItem::into_inner`, you can move the
query value out instead of cloning it.

In a web handler, use `HttpRequest::io()` to reach the connection. This example
returns the leaf certificate's SHA-256 thumbprint:

```rust,no_run
async fn certificate(req: ntex::web::HttpRequest) -> ntex::web::HttpResponse {
    use ntex::tls::{PeerCertChainDer, PeerCertDer};
    use ntex::tls::x509::{PeerCerts, digest::Algorithm};
    use ntex::web::HttpResponse;

    let Some(io) = req.io() else {
        return HttpResponse::Unauthorized().build();
    };
    let Some(leaf) = io.query::<PeerCertDer>().as_ref().cloned() else {
        return HttpResponse::Unauthorized().build();
    };
    let chain = io.query::<PeerCertChainDer>().as_ref().cloned()
        .map(|item| item.0).unwrap_or_default();
    match PeerCerts::from_der(leaf.0, chain) {
        Ok(peer) => HttpResponse::Ok().body(peer.leaf().thumbprint(Algorithm::Sha256)),
        Err(_) => HttpResponse::Forbidden().build(),
    }
}
```

This handler reports metadata; it does **not** decide whether the peer is
allowed to use your application. It handles both a missing connection, as can
happen with a synthetic request, and a connection without a peer certificate.
Keep endpoints like this protected: certificate fields and stable identity
identifiers should not be exposed as public diagnostics.

You can also read a `Certificate`'s subject, issuer, SANs, and validity dates.
Other TLS queries include `Servername`, `TlsVersion`, `TlsCipher`, and
`ntex::io::types::HttpProtocol`, depending on the backend and negotiated
session. `PskIdentity` is available only with OpenSSL.

## Trust and application authorization

Once you have a peer certificate, checking trust and deciding access are
separate steps. [`CaCerts`] handles client-certificate trust; it is not a
hostname verifier for outbound HTTPS connections.

Build the store from certificates your application explicitly trusts, called
*trust anchors*. Never build it from the chain a client sends you. Every
certificate you register becomes an anchor, even an intermediate or a pinned
leaf. An empty store trusts nothing.

Pay attention to import errors: `from_certs` logs and skips rejected anchors,
while `from_certs_with` lets you handle those rejections. The examples reject
an incomplete import rather than continuing with fewer roots than configured.

First verify the peer's chain at the current time, then check the application
identity. For example, this function accepts only the expected URI:

```rust
use std::time::SystemTime;
use ntex::tls::x509::{CaCerts, PeerCerts, TrustError, auth};

#[derive(Debug)]
enum AuthError {
    Trust(TrustError),
    Name(auth::Failure),
}

fn authorize(peer: &PeerCerts, trust: &CaCerts) -> Result<SystemTime, AuthError> {
    let expires = trust.is_trusted(peer, SystemTime::now())
        .map_err(AuthError::Trust)?;
    auth::name_matches(
        peer.leaf(),
        auth::NameField::Uri,
        "spiffe://ntex.example/client",
    )
    .map_err(AuthError::Name)?;
    Ok(expires)
}
```

The [shared example helpers][x509-example] use this policy. `AuthError` keeps
trust failures separate from name mismatches so callers can tell them apart.
The URI is an example application identity; matching it is not a complete
implementation of SPIFFE policy.

### Expiry and long-lived connections

`is_trusted` returns the earliest expiry in the verified certificate path,
including the trust anchor. Treat this as a deadline: an authenticated
connection must not stay open beyond it.

The examples schedule the connection to close at that time and cancel the
timer if it disconnects earlier. For long-lived sessions, also decide how your
application will respond to configuration changes and revoked identities.

### Other authorization checks

The `auth` helpers let you choose other identity checks:

| Helper | Behavior |
| --- | --- |
| `name_matches` | Matches any SAN of the chosen kind, or the first subject common name (CN); names are compared case-insensitively |
| `field` | Extracts one identity field; rejects multiple SAN values of the chosen kind |
| `validity` | Checks the certificate's validity window |
| `thumbprint_matches` | Matches an allow-list without checking dates |
| `verify_thumbprint` | Checks both the allow-list and validity |

Use SHA-256 for new thumbprint policies. SHA-1 is still useful for Windows
store selectors and compatibility, but these helpers do not verify certificate
chains, regardless of the digest you choose.

### Backend differences

`CaCerts` does not check revocation. Also, do not assume that Crypt32, WebPKI,
and OpenSSL accept exactly the same certificates. They differ in supported
algorithms, Extended Key Usage (EKU) checks for client authentication, and
how they handle a directly trusted leaf certificate.

For example, Windows checks validity rather than performing ordinary chain
validation for a pinned leaf. OpenSSL also handles EKU on intermediate
certificates differently from Crypt32 and WebPKI. Check the
[`CaCerts` documentation][CaCerts] when your policy needs to work consistently
across platforms.

## Operational guidance

### Handshake limits

Use `max_concurrent_ssl_accept` to limit how many TLS handshakes run at once
per worker. Acceptors on the same thread share the limit, which defaults to
256. Set it before starting workers: changing it later does not update counters
that other threads have already initialized.

This is a **soft** limit. Readiness slows down acceptance but does not reserve
a slot, so independently ready pipelines can exceed it. Setting the limit to
zero keeps acceptor readiness pending; zero does **not** mean unlimited.

Low-level handshake functions and filter constructors do not use this acceptor
limit and have their own timeout behavior. In particular, OpenSSL's free
`accept` and `connect` functions apply neither the service timeout nor its
concurrency limit. Use the services when you need those controls.

### Shutdown

TLS filters take part in normal, graceful I/O shutdown. Let the connection or
server shut down normally when possible. Forcing the transport to close can
lose queued bytes or prevent the TLS close notification from reaching the peer.

### Troubleshooting

When a connection fails, first check whether the problem is in TLS negotiation,
certificate verification, or the application protocol:

| Symptom | Check |
| --- | --- |
| `wss://` fails before the upgrade | Install a TLS connector on `WsClient`; the URL does not choose one |
| WebSocket upgrade fails after TLS | Offer HTTP/1.1, not HTTP/2 |
| Unknown issuer | Configure the verifier's roots; sending a root from the peer does not establish trust |
| Hostname mismatch | Check SANs and the request host separately from the destination address and SNI |
| No compatible rustls provider | Select a provider and configure X.509's provider/fallback too |
| Identity import fails | Check PKCS#8 encoding, encryption, key/leaf matching, and PKCS#12 backend support |
| Only some clients fail verification | Check missing intermediates, validity, EKU and platform policy |
| No peer certificate | Make sure the server requests one and the client explicitly supplies an identity |
| Schannel accepts an anonymous TLS handshake | Expected with its request-only setting; the application must reject missing identities |
| Windows store identity is unusable | Check store location, key permissions, selector and backend support for non-exportable keys |

Log failures without private keys or passwords. Treat peer certificate fields
as identity data, not harmless debugging output.

## Production checklist

Before you ship, walk through these questions:

- **Verification is on everywhere.** Every client, including the HTTP client
  with OpenSSL and every `WsClient`, uses a verifying configuration with the
  right trust roots. Nothing disables verification "temporarily."
- **Names match.** Server certificates list every hostname and IP address that
  clients use to connect.
- **Identities are real.** Example certificates and keys are not deployed.
  Private keys and PKCS#12 passwords come from protected storage and never
  appear in logs, command lines, or source control.
- **Renewal is planned.** You know when each certificate expires and how a
  replacement reaches the running service.
- **ALPN matches the protocol.** HTTP servers and clients offer the protocols
  they actually speak; WebSocket clients offer `http/1.1`.
- **Client authentication is enforced, not just requested.** For mutual TLS,
  missing certificates are rejected, client trust anchors are configured
  explicitly, and the application checks *which* identity connected, not only
  that its certificate is valid.
- **Long-lived connections respect expiry.** Authenticated connections close
  when their verified certificate path expires.
- **Handshakes are bounded.** The handshake timeout and
  `max_concurrent_ssl_accept` suit your expected load.

If every answer is yes, TLS is doing its job, and your handlers never had to
know it was there.

[examples]: https://github.com/ntex-rs/examples/tree/main/tls
[cert-setup]: https://github.com/ntex-rs/examples/blob/main/tls/README.md#local-certificates
[openssl-example]: https://github.com/ntex-rs/examples/tree/main/tls/openssl
[rustls-example]: https://github.com/ntex-rs/examples/tree/main/tls/rustls
[schannel-example]: https://github.com/ntex-rs/examples/tree/main/tls/schannel
[x509-example]: https://github.com/ntex-rs/examples/tree/main/tls/x509
[state-example]: https://github.com/ntex-rs/examples/tree/main/state/state-accumulation
[http-client-example]: https://github.com/ntex-rs/examples/tree/main/extras/http-client
[ws-example]: https://github.com/ntex-rs/examples/tree/main/websockets/basic
[`x509`]: https://docs.ntex.rs/api/ntex/tls/x509/index.html
[`TlsConfig`]: https://docs.ntex.rs/api/ntex/tls/struct.TlsConfig.html
[`PeerCertDer`]: https://docs.ntex.rs/api/ntex/tls/struct.PeerCertDer.html
[`PeerCertChainDer`]: https://docs.ntex.rs/api/ntex/tls/struct.PeerCertChainDer.html
[`CaCerts`]: https://docs.ntex.rs/api/ntex/tls/x509/struct.CaCerts.html
[CaCerts]: https://docs.ntex.rs/api/ntex/tls/x509/struct.CaCerts.html
