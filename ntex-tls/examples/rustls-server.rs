use std::{fs::File, io, io::BufReader, sync::Arc};

use ntex::{SharedCfg, codec, io::Io, server, service, util::Either};
use ntex_tls::rustls::TlsAcceptor;
use tls_rustls::ServerConfig;

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    println!("Started rustls echo server: 127.0.0.1:8443");

    // load ssl keys
    let cert_file = &mut BufReader::new(
        File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/cert.pem")).unwrap(),
    );
    let key_file = &mut BufReader::new(
        File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/key.pem")).unwrap(),
    );
    let keys = rustls_pemfile::private_key(key_file).unwrap().unwrap();
    let cert_chain = rustls_pemfile::certs(cert_file)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let tls_config = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, keys)
            .unwrap(),
    );

    // start server
    server::build()
        .bind(
            "basic",
            "127.0.0.1:8443",
            SharedCfg::new("S"),
            async move |_| {
                service(TlsAcceptor::new(tls_config.clone())).and_then(async move |io: Io<_>| {
                    println!("New client is connected");
                    loop {
                        match io.recv(&codec::BytesCodec).await {
                            Ok(Some(msg)) => {
                                println!("Got message: {:?}", msg);
                                io.send(msg, &codec::BytesCodec)
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
                })
            },
        )?
        .workers(1)
        .run()
        .await
}
