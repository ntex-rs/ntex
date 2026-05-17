#![allow(dead_code)]
use std::{fs::File, io::BufReader, sync::Arc};

use tls_rustls::ClientConfig;
use tls_rustls::pki_types::{CertificateDer, ServerName, UnixTime};

pub(crate) fn tls_connector() -> ClientConfig {
    ClientConfig::builder_with_protocol_versions(tls_rustls::ALL_VERSIONS)
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
        .with_no_client_auth()
}

pub(crate) fn tls_acceptor_arc() -> Arc<tls_rustls::ServerConfig> {
    Arc::new(tls_acceptor())
}

pub(crate) fn tls_acceptor() -> tls_rustls::ServerConfig {
    let cert_file = &mut BufReader::new(File::open("tests/cert.pem").unwrap());
    let key_file = &mut BufReader::new(File::open("tests/key.pem").unwrap());
    let cert_chain = rustls_pemfile::certs(cert_file)
        .map(|r| r.unwrap())
        .collect();
    let key = rustls_pemfile::private_key(key_file).unwrap().unwrap();
    tls_rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .unwrap()
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct NoCertificateVerification;

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
            tls_rustls::SignatureScheme::RSA_PKCS1_SHA1,
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
