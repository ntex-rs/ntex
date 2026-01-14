use std::io;

use ntex::io::types::PeerAddr;
use ntex::{
    Pipeline, ServiceFactory, SharedCfg, codec, connect, util::Bytes, util::Either,
};
use ntex_tls::TlsConfig;
use tls_rustls::{ClientConfig, RootCertStore};

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    // rustls config
    let cert_store =
        RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(cert_store)
        .with_no_client_auth();

    // rustls connector
    let connector = Pipeline::new(
        connect::rustls::TlsConnector::new(config.clone())
            .create(
                SharedCfg::new("CLIENT")
                    .add(TlsConfig::new().set_handshake_timeout(10))
                    .into(),
            )
            .await
            .unwrap(),
    );

    //let io = connector.call("www.rust-lang.org:443").await.unwrap();
    let io = connector.call("127.0.0.1:8443".into()).await.unwrap();
    println!("Connected to tls server {:?}", io.query::<PeerAddr>().get());
    io.send(Bytes::from_static(b"GET /\r\n\r\n"), &codec::BytesCodec)
        .await
        .map_err(Either::into_inner)?;

    let resp = io
        .recv(&codec::BytesCodec)
        .await
        .map_err(|e| e.into_inner())?
        .ok_or_else(|| io::Error::other("disconnected"))?;
    println!("Received: {:?}", resp);

    println!("disconnecting");
    io.shutdown().await
}
