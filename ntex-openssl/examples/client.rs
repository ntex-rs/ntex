use std::io;

use ntex::{codec, connect, util::Bytes, util::Either};
use openssl::ssl::{self, SslMethod, SslVerifyMode};

#[ntex::main]
async fn main() -> io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
    env_logger::init();

    println!("Connecting to openssl server: 127.0.0.1:8443");

    // load ssl keys
    let mut builder = ssl::SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();

    // start server
    let connector = connect::openssl::IoConnector::new(connector);

    let io = connector.connect("127.0.0.1:8443").await.unwrap();
    println!("Connected to ssl server");
    let result = io
        .send(Bytes::from_static(b"hello"), &codec::BytesCodec)
        .await
        .map_err(Either::into_inner)?;

    let resp = io
        .next(&codec::BytesCodec)
        .await
        .map_err(Either::into_inner)?;

    println!("disconnecting");
    io.shutdown().await
}
