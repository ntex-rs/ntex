use std::{io, sync::Arc};

use ntex::util::{Bytes, Either};
use ntex::{Pipeline, ServiceFactory, SharedCfg, codec, connect, io::types::PeerAddr};
use ntex_tls::TlsConfig;
use tls_rustls::ClientConfig;
use tls_rustls::pki_types::{CertificateDer, ServerName, UnixTime};

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    // rustls config
    let config = ClientConfig::builder_with_protocol_versions(tls_rustls::ALL_VERSIONS)
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertificateVerification {}))
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

#[derive(Debug)]
pub(crate) struct NoCertificateVerification {}

impl tls_rustls::client::danger::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _certs: &[CertificateDer<'_>],
        _hostname: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<tls_rustls::client::danger::ServerCertVerified, tls_rustls::Error> {
        Ok(tls_rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tls_rustls::DigitallySignedStruct,
    ) -> Result<tls_rustls::client::danger::HandshakeSignatureValid, tls_rustls::Error>
    {
        Ok(tls_rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tls_rustls::DigitallySignedStruct,
    ) -> Result<tls_rustls::client::danger::HandshakeSignatureValid, tls_rustls::Error>
    {
        Ok(tls_rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tls_rustls::SignatureScheme> {
        vec![
            tls_rustls::SignatureScheme::ECDSA_SHA1_Legacy,
            tls_rustls::SignatureScheme::RSA_PKCS1_SHA256,
            tls_rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            tls_rustls::SignatureScheme::RSA_PKCS1_SHA384,
            tls_rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            tls_rustls::SignatureScheme::RSA_PKCS1_SHA512,
            tls_rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            tls_rustls::SignatureScheme::RSA_PSS_SHA256,
            tls_rustls::SignatureScheme::RSA_PSS_SHA384,
            tls_rustls::SignatureScheme::RSA_PSS_SHA512,
            tls_rustls::SignatureScheme::ED25519,
            tls_rustls::SignatureScheme::ED448,
        ]
    }
}
