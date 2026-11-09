# Securing Connections with TLS

When your service leaves `localhost`, its traffic crosses networks you do not
control. TLS protects that traffic from being read or altered in transit. It
also lets peers authenticate each other so they can check who they are talking
to.

In ntex, you configure TLS when setting up a connection. Your HTTP handlers,
WebSocket services, and custom protocols still work with ordinary application
bytes.

All ntex TLS backends work on in-memory byte streams through the I/O filter
layer. They do not read from or write to sockets themselves. You can use them
with any ntex-supported I/O or runtime backend, provided the TLS backend
supports your platform.

The [`x509`] module gives you a platform-independent API for certificates and
identities. You use the same API to load identities, inspect certificates, and
check trust. ntex uses the appropriate platform or backend functionality
underneath.

If you are new to TLS, start with the
[HTTPS server](#quick-start-an-https-server), then read the
[certificate primer](#certificates-in-a-nutshell). If you already have a TLS
configuration, skip to [connecting it to ntex](#configuring-ntex-components).

For services that identify clients by certificate, the
[client-authentication section](#client-certificate-authentication) separates
TLS verification from application access checks. Use the
[production checklist](#production-checklist) before deploying either kind of
service.

The snippets focus on configuration. For complete programs, use the
[ntex examples repository][examples]. Each example's README includes its
dependencies, certificate setup, and run commands.

## Supported backends

ntex supports three TLS backends. Their types are available through `ntex::tls`,
or directly from `ntex_tls`.

| Backend | Enable it | Platform and configuration |
| --- | --- | --- |
| OpenSSL | `ntex/openssl` or `ntex-tls/openssl` | Platforms with OpenSSL; requires its native libraries and headers |
| rustls | `ntex/rustls` or `ntex-tls/rustls` | Configured with rustls `ClientConfig` and `ServerConfig`; choose a crypto provider |
| Schannel | Available automatically on Windows | Windows-native identities, trust policy, protocol versions and cipher suites |

Enable the corresponding feature to use OpenSSL or rustls. Neither is enabled
by default in `ntex-tls`. Schannel is available automatically on Windows; you do
not need its compatibility-only `schannel` feature.

For an OpenSSL application, start with:

```toml
[dependencies]
ntex = { version = "4", features = ["tokio", "openssl"] }
openssl = "0.10"
```

For rustls, you also need a crypto provider to implement the cryptographic
algorithms. The examples in this chapter use ring:

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

Choose a backend based on your platform and deployment needs. All three use
the same ntex integration.

For a new service, **rustls** is a good starting point. It is written in Rust
and needs no system TLS library, which simplifies cross-compilation and static
builds. You choose its crypto provider explicitly.

Choose **OpenSSL** if your organization already standardizes on it, you need to
reuse an existing configuration, or you need a feature only it provides here,
such as pre-shared-key (PSK) identities.

For a Windows-first deployment, **Schannel** uses the certificate stores and
trust policy already on the machine. It can also use keys without exporting
them.

Your protocol code does not depend on the backend. Switching later usually
requires changes only to the configuration code shown here.

### TLS and X.509 backend selection are different

You can choose different backends for TLS connections and certificate
operations. Enabling both TLS features lets you choose a transport for each
connection.

The [`x509`] helpers select their backend separately. On Windows, they always
use native APIs. Elsewhere, they use rustls when enabled and OpenSSL otherwise.
This means a certificate operation may not use the backend you chose for the
connection.

If verification fails, the helpers do not retry with another backend. The
`x509` module is always available on Windows. On other platforms, you must
enable OpenSSL or rustls; enabling `x509` alone is not enough.

On Unix, loading a PKCS#12 archive requires OpenSSL. This requirement applies
even if rustls handles your TLS connections and certificate verification.

## Quick start: an HTTPS server

This program serves one route over HTTPS. It uses rustls and the dependencies
listed above:

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

You need a certificate to run the server. Clone the
[examples repository][examples] and run `sh tls/generate-certs.sh` from its
root. Copy `server.pem`, `server.key`, and `ca.pem` from `tls/certs` into a
`certs` directory next to your project.

Start the server, then run this command in another terminal:

```sh
curl --cacert certs/ca.pem https://localhost:8443/
```

The `--cacert` option tells curl to trust the example certificate authority.
Without it, curl refuses the connection because it cannot verify the server
through an authority it trusts.

The application, routes, and handler stay the same as for a plain HTTP server.
The setup code loads an identity and builds a rustls `ServerConfig`. It then
calls `bind_rustls` instead of `bind`. The [rustls example][rustls-example]
provides a fuller version of this program.

## TLS as an I/O filter

TLS sits between the socket and your protocol. It decrypts incoming bytes
before your application sees them and encrypts the bytes your application
writes:

```text
socket <-> TLS <-> HTTP
socket <-> TLS <-> WebSocket <-> application protocol
```

Reads move from left to right in these diagrams; writes move in the opposite
direction. The filters transform buffers, while the I/O backend handles the
socket. The [I/O filter subsystem](7-io.md#filter-subsystem) chapter explains
this model in more detail.

Write your client or server protocol against ntex I/O to keep it independent
of the TLS backend. Using ntex I/O instead of a bare TCP socket leaves the
transport choice to connection setup.

On a server, the listener accepts a transport connection and a TLS acceptor
upgrades the existing `Io` through a handshake. On a client, a TLS connector
establishes the transport and performs the client handshake. After that, your
protocol service or codec handles application bytes without needing to know
which TLS backend is installed.

Adding TLS preserves any filters already installed on the connection. The type
`Io<Layer<TlsFilter, F>>` describes that stack. Your protocol can stay generic
over `F: Filter`.

To hide the concrete filter type, use `Io::seal` or `Io::boxed`. Encryption and
metadata remain available. Readiness and shutdown behavior also stay the same.

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
It echoes byte chunks rather than complete application messages. If your
protocol has message boundaries, use a framing codec instead of `BytesCodec`.

You can see this separation in the examples. The
[connection-state example][state-example] performs a handshake after accepting
a connection and before constructing its HTTP state. The
[WebSocket example][ws-example] uses the same frame handlers for plain and
encrypted connections.

If your application uses certificates to decide who may connect, its access
checks do need to inspect and verify the peer's certificate.

## Certificates in a nutshell

Understanding certificates helps you diagnose TLS failures. If you already
know how certificate chains work, skip ahead.

A **certificate** is a signed statement that a public key belongs to certain
names. Its subject alternative names (SANs) can include `DNS:api.example.com`
or `IP:10.0.0.5`. They can also contain a URI such as
`spiffe://example.org/billing`. The certificate records a validity period and
its **issuer**, the party that signed it.

The matching **private key** stays with its owner. During the handshake, a peer
proves that it holds the key. A stolen certificate alone cannot provide that
proof. In ntex, a certificate chain together with its private key is called an
**identity**.

Issuers are themselves identified by certificates, which forms a **chain**:

```text
leaf (api.example.com)  ->  intermediate CA  ->  root CA
```

The leaf identifies the endpoint. Intermediates connect it to a root. A root
is self-signed and proves nothing by itself. It is useful only because you
decided to trust it.

Locally configured trusted certificates are called **trust anchors**.
Certificate authorities (CAs) are the organizations or internal services that
issue certificates.

To verify a peer, a TLS implementation checks that:

1. the peer proves it owns the leaf's private key;
2. the chain leads from the leaf to one of your trust anchors;
3. every certificate in the path is within its validity period and allowed for
   this use;
4. for a server, the leaf's names include the host the client meant to reach.

Nearly every certificate failure corresponds to one of these steps. An unknown
issuer, an expired certificate, or a name mismatch can each prevent a
connection. Use these checks to interpret error messages, then fix the
configuration rather than disabling verification.

## Configuring a backend

Your TLS configuration defines how peers identify and verify each other. It
also controls which application protocols they can negotiate.

Start with the identity you will present: a certificate chain and its private
key. Then choose the certificate authorities you trust to identify the other
side. A client also needs the server name it expects to reach. Finally, list
the application protocols you support. TLS chooses between those protocols
through Application-Layer Protocol Negotiation (ALPN).

The connector normally checks the host from the request, not just the address
it connects to. A certificate for `localhost` does not also identify
`127.0.0.1` unless it lists that IP address. Server Name Indication (SNI) helps
the server choose which certificate to present. SNI does not replace the
client's verification.

To try the examples, first run [`tls/generate-certs.sh`][cert-setup] from the
examples repository root:

```sh
sh tls/generate-certs.sh
```

The script creates a local certificate authority (CA) and certificates with
subject alternative names (SANs). These names identify hosts or application
identities. The script also creates unencrypted PKCS#8 private keys. The files
are gitignored and meant only for local development; never deploy them.

Run each program from its own example directory so the relative certificate
paths resolve correctly. The following sections build the client and server
configurations separately. Then attach them to your application as shown in
[configuring ntex components](#configuring-ntex-components).

### OpenSSL

To use OpenSSL, configure client verification and the server's identity
separately. The [OpenSSL example][openssl-example] includes an HTTPS server
and client, plus a mutual TLS (mTLS) server that authenticates clients.

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

`set_identity` installs the private key and the endpoint's certificate (the
leaf). It includes intermediate certificates in chain order and leaves out
the self-signed root. It does not change how peers are verified.

The `mozilla_intermediate_v5` preset supports modern TLS, including TLS 1.3.
You can pass these configurations to the HTTP helpers below. For direct use,
build a `ntex::tls::openssl::SslConnector` service or an `SslAcceptor` service.
If you are building a custom protocol, set its ALPN instead of copying the HTTP
protocol list.

### rustls

To configure rustls, choose a crypto provider and the certificates you trust.
The [rustls example][rustls-example] selects ring explicitly for TLS. It also
enables `rustls-ring` for the portable identity and trust helpers.

Loading or converting an identity does not install a process-wide crypto
provider. A client needs both a provider and a root store:

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

If your dependencies enable more than one rustls crypto provider, choose the
process-wide provider explicitly. Install it before using a default builder
that needs a provider. `builder_with_provider` configures only that builder.
It does not select a provider for other builders or X.509 operations.

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

This client offers `h2` and `http/1.1` and sends no client certificate.
To send one, call `set_client_cert` explicitly. Schannel does not choose an
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

This client configuration has no separate root store for verifying servers.
For local development, follow the example README to add the example CA in an
isolated Windows account, then remove it afterward. Keep verification enabled
and configure Windows trust rather than bypassing it.

## Configuring ntex components

Once you have a backend configuration, attach it to the client or server that
opens the connection. The APIs differ slightly between HTTP and WebSocket
clients, so use the section that matches your application.

### HTTP client

For HTTPS, install a secure connector on the client builder. Pass an OpenSSL
configuration to `ClientBuilder::openssl(config)`. For rustls, use
`ClientBuilder::rustls(config)`. Request and response handling stay the same:

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

Build the client once and reuse it. Its connection pool lets later requests
to the same host skip the TLS handshake.

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

`ClientBuilder::connector` configures **unsecured** connections. It leaves
the HTTPS connector unchanged.

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

When wrapping connection errors, preserve the original error in an application
error type or `Box<dyn std::error::Error>`. Avoid converting it to an
`io::Error` that contains only a string.

### WebSocket client

`WsClient` has its own connector and does not inherit settings from an HTTP
`Client`. Its default connector uses plain TCP: **a `wss://` URL alone does
not enable TLS.**

Install a TLS connector explicitly. These WebSocket APIs use an HTTP/1.1
upgrade rather than HTTP/2 extended CONNECT. Offer `http/1.1` instead of `h2`:

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

With rustls, set `config.alpn_protocols = vec![b"http/1.1".to_vec()]`.
Then pass the configuration to `.rustls(Arc::new(config))`.

With Schannel, restrict ALPN in `ClientConfig`. Pass the configuration to
`.connector(TlsConnector::with_config(config))`.

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

A WebSocket connection starts as HTTP. Use an HTTPS listener and keep your
upgrade and frame handlers. TLS stays in place after the upgrade;
`web::ws::start` does not remove it.

Use the lower-level server builder when you need Schannel, a custom protocol,
or a certificate check before serving any requests. Compose the services in
the order you want them to run:

```text
listener -> TLS acceptor -> optional authorization gate -> HttpService / protocol service
```

There is no `bind_schannel` web-server helper. The
[Schannel server][schannel-example] and both mutual-authentication examples
show this service chain. Their HTTP service does not depend on the concrete
TLS filter. They use a shared boxed error type to preserve errors from the
handshake, authentication, and HTTP layers.

### Shared handshake configuration

Use [`TlsConfig`] to control how long a TLS handshake may take. Attach it to
the server's accepted I/O configuration or the client connector's `SharedCfg`:

```rust
use ntex::{SharedCfg, time::Seconds, tls::TlsConfig};

let cfg: SharedCfg = SharedCfg::new("TLS")
    .add(TlsConfig::new().set_handshake_timeout(Seconds(10)))
    .into();
```

Pass `cfg` to the server's bind/listen call, to `ClientBuilder::build` for an
HTTP client, or to `WsClient::new` for a WebSocket client. The default TLS
handshake timeout is five seconds; zero disables the deadline.

This timeout covers only TLS negotiation. DNS lookup, establishing a TCP
connection, waiting for an HTTP response, and the WebSocket opening handshake
have separate settings in their respective components.

## X.509 certificates and identity loading

Use the [`x509`] API to load certificates and identities across platforms.
Supported features and verification behavior can still differ by backend.
The API distinguishes certificates, identities, and trust anchors:

| Type | Meaning |
| --- | --- |
| `Certificate` | A parsed public certificate, without a private key |
| `Identity` / `ClientIdentity` | Certificate chain plus a matching private key |
| `CaCerts` | Certificates you explicitly trust when verifying clients |

Loading an identity checks that its data is well-formed and the key matches
the certificate. It does not establish trust or grant access. It also does not
check whether the identity is valid for a particular hostname. Parsing a
certificate reads its contents without authenticating whoever sent it.

The [X.509 example][x509-example] includes an inspector you can run against your
files, plus the authentication helpers shared by all three backend examples.

### PEM and DER

PEM stores certificate data as text; DER stores it as binary. Load a PEM
identity with `Identity::from_pem(certificates, private_key)`. Put the leaf
certificate first in the chain and supply an **unencrypted PKCS#8** private
key.

For DER inputs, use `Identity::from_der(&[&[u8]], private_key)`. Convert PKCS#1,
SEC1, or encrypted PEM keys before using these portable loaders.

If you only need to inspect a certificate, you do not need a private key.
Use `Certificate::from_pem` for one PEM certificate, `from_pem_multi` for a
PEM bundle, or `Certificate::from_der` for owned DER bytes.

For an identity stored in files, `IdentitySource` handles the file reads for
you:

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

PKCS#12 archives bundle certificates and private keys. For a password-protected
archive, use `Identity::from_pkcs12(bytes, password)`. The file loader,
`IdentitySource::Pkcs12File(path)`, uses an empty password.

Parsing a plain string as `IdentitySource` treats it as a PKCS#12 path,
**not** a PEM path. Load passwords from a secret source or protected environment
configuration. Do not pass them as command-line arguments or write them to logs.

PKCS#12 loading uses native APIs on Windows and requires the `openssl` feature
on Unix. You can still use rustls for TLS; using OpenSSL to import an archive
does not choose the connection's TLS backend.

On Windows, you can also select an identity and its key from a certificate
store:

```text
cert:CurrentUser\MY\<40-hex-SHA1-thumbprint>
cert:LocalMachine\MY\subject=<name>
```

A thumbprint selector does not check whether the certificate is currently
valid. A subject selector picks a currently valid match with an accessible
private key. If several match, it chooses the one with the latest expiry.
Neither selector verifies trust, and both work only on Windows.

Windows keys may be non-exportable. A backend can use such a key to sign
without reading its bytes.

When importing an identity, consider how long the key stays on the system.
`Identity` keeps an isolated imported key for Schannel until its last owner is
dropped, then normally deletes it. An abnormal process exit can leave the key
behind. `ClientIdentity` imports use ephemeral native keys instead, which is
useful when only a client needs the identity.

### Using an identity with each backend

Once you have loaded an identity, adapt it to your TLS backend. The adapter
determines whether the backend can use the identity's private key.

| Backend | Adapter | Important limitation |
| --- | --- | --- |
| OpenSSL | `ntex::tls::openssl::set_identity(&mut context, &identity)` | Requires an exportable PKCS#8 key; cannot use opaque Windows CNG keys through this adapter |
| rustls | `certified_key(&identity)` or `client_certified_key(&client_identity)` | Install the result through a certificate resolver; native Windows keys can sign without being exported |
| Schannel | `identity.schannel().clone()` | Windows-only native identity for client/server configuration |

For rustls, use `SingleCertAndKey` when you have one configured identity. It
works for both clients and servers.

To prepare an ordered client chain from an existing identity, use
`ClientIdentity::from_identity`. You can also load a `ClientIdentity` directly
from PEM, DER, or PKCS#12.

The OpenSSL and rustls adapters order the chain for you. They use
`Identity::ordered_chain(false)` to arrange the intermediates and leave out the
self-signed root. It drops unrelated certificates with a warning.

Sending a root certificate does not make the other side trust it. Trusted
certificates belong in the verifier's configuration, not in a chain supplied
by the peer.

## Client-certificate authentication

Use mutual TLS when clients need to prove their identities with certificates.
Ordinary HTTPS authenticates the server to the client. Mutual TLS also asks the
client to present a certificate and prove that it owns the corresponding
private key.

Your application still needs to answer three separate questions:

1. Did the client provide a certificate?
2. Is that certificate trusted?
3. Is that identity allowed to use this application?

Start by configuring the server to ask for or require a client certificate:

| Backend | Requiring or requesting a client identity |
| --- | --- |
| OpenSSL | Set `PEER \| FAIL_IF_NO_PEER_CERT` and configure client trust anchors |
| rustls | Configure `WebPkiClientVerifier` with client roots; do not enable anonymous clients when a certificate is mandatory |
| Schannel | `request_client_cert(true)` requests an optional certificate and proof of key possession; the application must require and verify it |

The client needs its own identity as well as roots for checking the server.
Install that identity with OpenSSL's `set_identity`, a rustls certificate
resolver, or Schannel's `set_client_cert`. Keep these two parts of the
configuration separate: sending a client certificate does not replace
checking the server's name and certificate chain.

In OpenSSL, `PEER` alone does not require the client to send a certificate.
In rustls, making the verifier optional is not the same as turning off client
authentication altogether.

**Schannel only requests a certificate; it does not enforce your authentication
policy.** The handshake can succeed without a certificate. It also does not
check trust in the client's certificate here. Your application must reject
missing or unacceptable identities before doing any protected work.

The example checks the identity before `HttpService`. A rejection therefore
closes the connection without sending an HTTP response.

To try the OpenSSL example, run these from `tls/openssl` in separate terminals:

```sh
cargo run --bin openssl-mtls-server
cargo run --bin openssl-client -- https://localhost:8443/ ../certs/client
```

With the expected client identity, the response is `authorized`. Try again
without an identity, with `untrusted-client`, and with `other-client`. Each
connection should be rejected.

The `other-client` certificate is valid and comes from a trusted issuer. Its
identity is still not one this application allows.

The rustls and Schannel READMEs provide equivalent commands.

## Reading peer certificates

Use [`PeerCertDer`] to read the peer's leaf certificate. Use
[`PeerCertChainDer`] to read its certificate chain. These queries work across
backends; native certificate-chain queries follow different conventions.

This helper works with `IoRef` from any supported TLS backend:

```rust
use ntex::io::IoRef;
use ntex::tls::{PeerCertChainDer, PeerCertDer};
use ntex::tls::x509::{self, PeerCerts};

fn peer_certs(io: &IoRef) -> Result<Option<PeerCerts>, x509::Error> {
    let Some(leaf) = io.query::<PeerCertDer>().into_inner() else {
        return Ok(None);
    };
    let chain = io.query::<PeerCertChainDer>()
        .into_inner()
        .map(|item| item.0)
        .unwrap_or_default();
    PeerCerts::from_der(leaf.0, chain).map(Some)
}
```

`QueryItem::into_inner()` moves out the query values, which own their DER
buffers. `PeerCerts::from_der` parses those buffers. It removes a duplicated
leaf if one appears at the start of the supplied chain.

`PeerCerts::chain` holds the remaining chain. That chain may be incomplete or
untrusted.

In a web handler, use `HttpRequest::io()` to reach the connection. The
`peer_certs` helper above then parses its certificates. This example returns
the leaf certificate's SHA-256 thumbprint:

```rust,no_run
async fn certificate(req: ntex::web::HttpRequest) -> ntex::web::HttpResponse {
    use ntex::tls::x509::digest::Algorithm;
    use ntex::web::HttpResponse;

    let Some(io) = req.io() else {
        return HttpResponse::Unauthorized().build();
    };
    match peer_certs(io) {
        Ok(Some(peer)) => HttpResponse::Ok().body(peer.leaf().thumbprint(Algorithm::Sha256)),
        Ok(None) => HttpResponse::Unauthorized().build(),
        Err(_) => HttpResponse::Forbidden().build(),
    }
}
```

This handler reports metadata. It does **not** decide whether the peer is
allowed to use your application.

The two `Unauthorized` paths cover a synthetic request with no connection
and a real connection without a peer certificate.

Protect endpoints like this. Do not expose certificate fields or stable
identity identifiers as public diagnostics.

You can also read a `Certificate`'s subject, issuer, SANs, and validity dates.
For connection metadata, query `Servername`, `TlsVersion`, `TlsCipher`, or
`ntex::io::types::HttpProtocol`. Their availability depends on the backend and
negotiated session. `PskIdentity` is available only with OpenSSL.

## Trust and application authorization

Once you have a peer certificate, checking trust and deciding access are
separate steps. [`CaCerts`] handles client-certificate trust; it is not a
hostname verifier for outbound HTTPS connections.

Build the store from certificates your application explicitly trusts, called
*trust anchors*. Never build it from the chain a client sends you. Every
certificate you register becomes an anchor, even an intermediate or a pinned
leaf. An empty store trusts nothing.

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

Check for import errors. The `from_certs` method logs and skips rejected
anchors. Use `from_certs_with` to handle those rejections yourself. The examples
reject an incomplete import rather than continuing with fewer roots than
configured.

### Expiry and long-lived connections

Close authenticated connections when their verified certificate path
expires. The `is_trusted` method returns the earliest expiry in that path,
including the trust anchor. An authenticated connection must not stay open
beyond that deadline.

The examples schedule the connection to close at that time and cancel the
timer if it disconnects earlier. For long-lived sessions, also decide how your
application will respond to configuration changes and revoked identities.

### Other authorization checks

Use the `auth` helpers to choose the identity fields or thumbprints your
application accepts. Some helpers also check certificate validity:

| Helper | Behavior |
| --- | --- |
| `name_matches` | Matches any SAN of the chosen kind, or the first subject common name (CN); names are compared case-insensitively |
| `field` | Extracts one identity field; rejects multiple SAN values of the chosen kind |
| `validity` | Checks the certificate's validity window |
| `thumbprint_matches` | Matches an allow-list without checking dates |
| `verify_thumbprint` | Checks both the allow-list and validity |

Use SHA-256 for new thumbprint policies. SHA-1 remains useful for Windows
store selectors and compatibility. These helpers do not verify certificate
chains, regardless of the digest you choose.

### Backend differences

If your application runs on several platforms, check how each backend applies
your trust policy. Crypt32, WebPKI, and OpenSSL do not necessarily accept the
same certificates. They differ in supported algorithms and Extended Key Usage
(EKU) checks for client authentication. They also handle directly trusted leaf
certificates differently.

`CaCerts` does not check revocation.

For example, Windows checks validity rather than performing ordinary chain
validation for a pinned leaf. OpenSSL also handles EKU on intermediate
certificates differently from Crypt32 and WebPKI. Check the
[`CaCerts` documentation][CaCerts] when your policy needs to work consistently
across platforms.

## Operational guidance

TLS configuration also affects how your service handles load and closes
connections.

### Handshake limits

TLS handshakes cost work before your application handles any requests. Use
`max_concurrent_ssl_accept` to limit concurrent handshakes per worker.
Acceptors on the same thread share the limit, which defaults to 256. Set it
before starting workers. Later changes do not update counters that other
threads have already initialized.

This is a **soft** limit. Readiness slows acceptance without reserving a slot,
so independently ready pipelines can exceed it. Setting the limit to zero
keeps acceptor readiness pending. Zero does **not** mean unlimited.

Low-level handshake functions and filter constructors do not use this acceptor
limit. They have their own timeout behavior. OpenSSL's free `accept` and
`connect` functions apply neither the service timeout nor its concurrency
limit. Use the services when you need those controls.

### Shutdown

Let the connection or server shut down gracefully when possible. TLS filters
participate in normal I/O shutdown. Forcing the transport to close can lose
queued bytes or prevent the TLS close notification from reaching the peer.

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

Before deploying, check each item:

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
  reject missing certificates and configure client trust anchors explicitly.
  Check *which* identity connected; a valid certificate alone does not grant
  access.
- **Long-lived connections respect expiry.** Authenticated connections close
  when their verified certificate path expires.
- **Handshakes are bounded.** The handshake timeout and
  `max_concurrent_ssl_accept` suit your expected load.

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
