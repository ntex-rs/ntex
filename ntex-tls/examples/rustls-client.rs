use std::io;

use ntex::{codec, connect, io::types::PeerAddr, util::Bytes, util::Either};
use tls_rust::{ClientConfig, OwnedTrustAnchor, RootCertStore};

#[ntex::main]
async fn main() -> io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
    env_logger::init();

    // rustls config
    let mut cert_store = RootCertStore::empty();
    cert_store.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
        OwnedTrustAnchor::from_subject_spki_name_constraints(
            ta.subject,
            ta.spki,
            ta.name_constraints,
        )
    }));
    let config = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(cert_store)
        .with_no_client_auth();

    // rustls connector
    let connector = connect::rustls::Connector::new(config.clone());

    //let io = connector.connect("www.rust-lang.org:443").await.unwrap();
    let io = connector.connect("127.0.0.1:8443").await.unwrap();
    println!("Connected to tls server {:?}", io.query::<PeerAddr>().get());
    let result = io
        .send(Bytes::from_static(b"GET /\r\n\r\n"), &codec::BytesCodec)
        .await
        .map_err(Either::into_inner)?;
    println!("Send result: {:?}", result);

    let resp = io
        .recv(&codec::BytesCodec)
        .await
        .map_err(|e| e.into_inner())?
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "disconnected"))?;
    println!("Received: {:?}", resp);

    println!("disconnecting");
    io.shutdown().await
}
