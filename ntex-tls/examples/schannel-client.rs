//! Windows Schannel echo client, connects to the `server` or `rustls-server`
//! example.
//!
//! Set `CLIENT_CERT_SUBJECT` to send a client certificate from the current
//! user's personal store, for example `CLIENT_CERT_SUBJECT=client.example.com`.
use std::io;

#[cfg(windows)]
#[ntex::main]
async fn main() -> io::Result<()> {
    use ntex::util::{Bytes, Either};
    use ntex::{Pipeline, SharedCfg, codec, connect::Connect, connect::Connector};
    use ntex_tls::schannel::{CertStoreLocation, ClientCert, ClientConfig, TlsConnector};

    env_logger::init();

    println!("Connecting to tls server: localhost:8443");

    // the example certificate is self-signed
    let mut config = ClientConfig::new().danger_accept_invalid_certs(true);
    if let Ok(subject) = std::env::var("CLIENT_CERT_SUBJECT") {
        let cert =
            ClientCert::from_store_by_subject(CertStoreLocation::CurrentUser, "MY", &subject)?;
        config = config.set_client_cert(cert);
    }

    let connector = Pipeline::new(
        SharedCfg::default(),
        TlsConnector::<Connector<&'static str>>::with_config(config),
    );
    let io = connector
        .call(Connect::new("localhost").set_addr(Some(([127, 0, 0, 1], 8443).into())))
        .await
        .map_err(io::Error::other)?;
    println!("Connected to tls server");

    io.send(Bytes::from_static(b"hello"), &codec::BytesCodec)
        .await
        .map_err(Either::into_inner)?;
    let resp = io
        .recv(&codec::BytesCodec)
        .await
        .map_err(Either::into_inner)?
        .ok_or_else(|| io::Error::other("disconnected"))?;
    println!("Received: {resp:?}");

    println!("disconnecting");
    io.shutdown().await
}

#[cfg(not(windows))]
fn main() -> io::Result<()> {
    Err(io::Error::other("schannel is available on windows only"))
}
