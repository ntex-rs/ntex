use std::{fs::File, io, io::BufReader, sync::Arc};

use ntex::service::{chain_factory, fn_service};
use ntex::{codec, io::filter, io::Io, server, util::Either};
use ntex_tls::rustls::TlsAcceptor;
use rustls_pemfile::{certs, rsa_private_keys};
use tls_rust::{Certificate, PrivateKey, ServerConfig};

#[ntex::main]
async fn main() -> io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
    env_logger::init();

    println!("Started openssl echp server: 127.0.0.1:8443");

    // load ssl keys
    let cert_file =
        &mut BufReader::new(File::open("../ntex-tls/examples/cert.pem").unwrap());
    let key_file = &mut BufReader::new(File::open("../ntex-tls/examples/key.pem").unwrap());
    let keys = PrivateKey(rsa_private_keys(key_file).unwrap().remove(0));
    let cert_chain = certs(cert_file)
        .unwrap()
        .iter()
        .map(|c| Certificate(c.to_vec()))
        .collect();
    let tls_config = Arc::new(
        ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(cert_chain, keys)
            .unwrap(),
    );

    // start server
    server::ServerBuilder::new()
        .bind("basic", "127.0.0.1:8443", move |_| {
            chain_factory(filter(TlsAcceptor::new(tls_config.clone()))).and_then(
                fn_service(|io: Io<_>| async move {
                    println!("New client is connected");

                    io.send(
                        ntex_bytes::Bytes::from_static(b"Welcome!\n"),
                        &codec::BytesCodec,
                    )
                    .await
                    .map_err(Either::into_inner)?;

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
