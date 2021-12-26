use std::io;

use ntex::service::{fn_service, pipeline_factory};
use ntex::{codec, io::add_filter, io::Io, server, util::Either};
use ntex_tls::openssl::SslAcceptor;
use tls_openssl::ssl::{self, SslFiletype, SslMethod};

#[ntex::main]
async fn main() -> io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
    env_logger::init();

    println!("Started openssl echp server: 127.0.0.1:8443");

    // load ssl keys
    let mut builder = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./examples/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./examples/cert.pem")
        .unwrap();
    let acceptor = builder.build();

    // start server
    server::ServerBuilder::new()
        .bind("basic", "127.0.0.1:8443", move |_| {
            pipeline_factory(add_filter(SslAcceptor::new(acceptor.clone()))).and_then(
                fn_service(|io: Io<_>| async move {
                    println!("New client is connected");
                    loop {
                        match io.recv(&codec::BytesCodec).await {
                            Ok(Some(msg)) => {
                                println!("Got message: {:?}", msg);
                                io.send(msg.freeze(), &codec::BytesCodec)
                                    .await
                                    .map_err(Either::into_inner)?;
                            }
                            Err(e) => {
                                println!("Got error: {:?}", e);
                                break;
                            }
                            Ok(None) => break,
                        }
                    }
                    println!("Client is disconnected");
                    Ok(())
                }),
            )
        })?
        .workers(1)
        .run()
        .await
}
