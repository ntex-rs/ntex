//! Certificates presented by a TLS peer.
use ntex_bytes::Bytes;

use crate::x509::{Certificate, Error};

/// Leaf certificate and additional chain certificates presented by a TLS peer.
///
/// Built from ntex-tls `PeerCertDer` / `PeerCertChainDer` (moved out of the io query with
/// `QueryItem::into_inner`, so no copy). Presence of a chain does **not** imply trust,
/// use [`crate::x509::trust::CaCerts::is_trusted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCerts {
    leaf: Certificate,
    chain: Vec<Certificate>,
}

impl PeerCerts {
    /// Parses the peer leaf certificate and chain, taking ownership of the buffers.
    ///
    /// `chain` is expected leaf-first in the order sent by the peer. If its first element is
    /// the leaf itself (ntex-tls OpenSSL server side) it is removed, so [`Self::chain`]
    /// never contains the leaf.
    ///
    /// # Errors
    /// [`Error::InvalidDer`] if the leaf or any chain certificate cannot be parsed.
    pub fn from_der(leaf: Bytes, chain: Vec<Bytes>) -> Result<Self, Error> {
        let mut chain = chain.into_iter().peekable();
        if chain.peek().is_some_and(|first| *first == leaf) {
            chain.next();
        }
        let chain = chain
            .map(Certificate::from_der)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PeerCerts {
            leaf: Certificate::from_der(leaf)?,
            chain,
        })
    }

    /// Creates peer certificates from already parsed certificates.
    ///
    /// `chain` must not contain the leaf, leaf issuer first.
    #[must_use]
    pub fn new(leaf: Certificate, chain: Vec<Certificate>) -> Self {
        PeerCerts { leaf, chain }
    }

    /// Peer (leaf) certificate.
    #[must_use]
    pub fn leaf(&self) -> &Certificate {
        &self.leaf
    }

    /// Additional certificates sent by the peer, leaf excluded, leaf issuer first.
    #[must_use]
    pub fn chain(&self) -> &[Certificate] {
        &self.chain
    }
}

#[cfg(all(test, unix, feature = "openssl"))]
mod tests {
    use super::*;
    use crate::x509::cert::tests::testgen;

    /// STATEMENT: Peer parsing removes the redundant leaf but retains issuer certificates.
    ///
    /// HOW:
    /// - Parse chains with and without a repeated leaf, an empty chain, and malformed DER.
    #[test]
    fn strips_leaf_from_chain() {
        let k = testgen::key();
        let ca = testgen::cert("ca", true, None, &k, None, (-1, 1));
        let k2 = testgen::key();
        let leaf = testgen::cert("leaf", false, Some((&ca, &k)), &k2, None, (-1, 1));
        let leaf_der = Bytes::from(leaf.to_der().unwrap());
        let ca_der = Bytes::from(ca.to_der().unwrap());

        let p =
            PeerCerts::from_der(leaf_der.clone(), vec![leaf_der.clone(), ca_der.clone()]).unwrap();
        assert_eq!(p.leaf().subject_cn(), Some("leaf"));
        assert_eq!(p.chain().len(), 1);
        assert_eq!(p.chain()[0].subject_cn(), Some("ca"));

        let p = PeerCerts::from_der(leaf_der.clone(), vec![ca_der]).unwrap();
        assert_eq!(p.chain().len(), 1);
        let p = PeerCerts::from_der(leaf_der.clone(), vec![]).unwrap();
        assert!(p.chain().is_empty());
        assert!(PeerCerts::from_der(leaf_der, vec![Bytes::from_static(b"x")]).is_err());
    }
}
