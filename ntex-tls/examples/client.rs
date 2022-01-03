use std::io;

use ntex::{codec, connect, util::Bytes, util::Either};
use tls_openssl::ssl::{self, SslFiletype, SslMethod, SslVerifyMode};

#[ntex::main]
async fn main() -> io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
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
    let connector = connect::openssl::Connector::new(builder.build());

    let io = connector.connect("127.0.0.1:8443").await.unwrap();
    println!("Connected to ssl server");
    let result = io
        .send(Bytes::from_static(b"hello"), &codec::BytesCodec)
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
