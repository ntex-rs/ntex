use std::io;

use ntex::service::{Pipeline, ServiceFactory};
use ntex::{SharedCfg, codec, connect, util::Bytes, util::Either};
use tls_openssl::ssl::{self, SslFiletype, SslMethod, SslVerifyMode};

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    println!("Connecting to openssl server: 127.0.0.1:8443");

    // load ssl keys
    let mut builder = ssl::SslConnector::builder(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./examples/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./examples/cert.pem")
        .unwrap();
    builder.set_verify(SslVerifyMode::NONE);

    // openssl connector
    let connector = Pipeline::new(
        connect::openssl::SslConnector::new(builder.build())
            .create(SharedCfg::default())
            .await
            .unwrap(),
    );

    let io = connector.call("127.0.0.1:8443".into()).await.unwrap();
    println!("Connected to ssl server");
    io.send(Bytes::from_static(b"hello"), &codec::BytesCodec)
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
