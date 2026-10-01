//! Windows Schannel echo server, the `schannel-client` and `rustls-client`
//! examples connect to it.
//!
//! Set `REQUEST_CLIENT_CERT=1` to request a certificate from clients.
use std::io;

#[cfg(windows)]
#[ntex::main]
async fn main() -> io::Result<()> {
    use ntex::{SharedCfg, codec, io::Io, server, service, util::Either};
    use ntex_tls::schannel::{Certificate, PeerCert, ServerConfig, TlsAcceptor};

    env_logger::init();

    println!("Started schannel echo server: 127.0.0.1:8443");

    // certificate and key of `cert.pem` and `key.pem`
    let cert = Certificate::from_pkcs12(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/identity.pfx"
        )),
        "ntex",
    )?;
    let config = ServerConfig::new(cert)?
        .request_client_cert(std::env::var_os("REQUEST_CLIENT_CERT").is_some());

    // start server
    server::build()
        .bind(
            "basic",
            "127.0.0.1:8443",
            SharedCfg::new("S"),
            async move |_| {
                service(TlsAcceptor::new(config.clone())).and_then(async move |io: Io<_>| {
                    let client_cert = io.query::<PeerCert>().as_ref().is_some();
                    println!("New client is connected, client certificate: {client_cert}");
                    loop {
                        match io.recv(&codec::BytesCodec).await {
                            Ok(Some(msg)) => {
                                println!("Got message: {msg:?}");
                                io.send(msg, &codec::BytesCodec)
                                    .await
                                    .map_err(Either::into_inner)?;
                            }
                            Err(e) => {
                                println!("Got error: {e:?}");
                                break;
                            }
                            Ok(None) => break,
                        }
                    }
                    println!("Client is disconnected");
                    Ok(())
                })
            },
        )?
        .workers(1)
        .run()
        .await
}

#[cfg(not(windows))]
fn main() -> io::Result<()> {
    Err(io::Error::other("schannel is available on windows only"))
}
