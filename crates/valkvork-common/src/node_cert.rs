//! Per-node client certificates (FR-SEC-02).
//!
//! The controller runs a small CA, the *node CA*, whose only job is to sign
//! agents' client certificates. A node certificate's common name is the
//! node's id, and it is valid for client authentication only. Agents send a
//! certificate signing request (CSR) over their authenticated stream; the
//! controller takes only the public key from it and sets everything else.

use crate::errors::ValkvorkError;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SerialNumber,
};
use std::time::Duration;
use time::OffsetDateTime;
use x509_parser::prelude::{FromDer, X509Certificate, X509CertificationRequest};

const CA_COMMON_NAME: &str = "Valkvork node CA";
/// The CA outlives any deployment that keeps its database.
const CA_LIFETIME: Duration = Duration::from_secs(20 * 365 * 24 * 3600);
/// Tolerates clock skew between the controller and agents.
const BACKDATE: Duration = Duration::from_secs(5 * 60);

fn tls_err(what: &str) -> impl Fn(rcgen::Error) -> ValkvorkError + '_ {
    move |e| ValkvorkError::Tls(format!("{what}: {e}"))
}

fn dn(common_name: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    dn
}

/// The controller's node CA: its certificate and signing key.
pub struct NodeCa {
    cert_pem: String,
    issuer: rcgen::Certificate,
    key: KeyPair,
}

/// A certificate the node CA issued.
#[derive(Debug, Clone)]
pub struct IssuedCert {
    pub cert_pem: String,
    pub serial: String,
    pub not_after_ms: i64,
}

impl NodeCa {
    /// A new CA with a fresh P-256 key.
    pub fn generate() -> Result<Self, ValkvorkError> {
        let key = KeyPair::generate().map_err(tls_err("generate node CA key"))?;
        let now = OffsetDateTime::now_utc();
        let mut p = CertificateParams::default();
        p.distinguished_name = dn(CA_COMMON_NAME);
        p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        p.not_before = now - BACKDATE;
        p.not_after = now + CA_LIFETIME;
        p.serial_number = Some(random_serial());
        let issuer = p.self_signed(&key).map_err(tls_err("sign node CA"))?;
        Ok(Self {
            cert_pem: issuer.pem(),
            issuer,
            key,
        })
    }

    /// Load a CA stored by [`NodeCa::cert_pem`] and [`NodeCa::key_pem`].
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, ValkvorkError> {
        let key = KeyPair::from_pem(key_pem).map_err(tls_err("node CA key"))?;
        let params = CertificateParams::from_ca_cert_pem(cert_pem)
            .map_err(tls_err("node CA certificate"))?;
        // Re-signing yields an issuer with the stored CA's name and key
        // identifier; certificates it signs chain to the stored certificate.
        let issuer = params.self_signed(&key).map_err(tls_err("node CA"))?;
        Ok(Self {
            cert_pem: cert_pem.to_string(),
            issuer,
            key,
        })
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn key_pem(&self) -> String {
        self.key.serialize_pem()
    }

    /// Sign a client certificate for `node_id` with the public key in
    /// `csr_pem`. The CSR must be signed by its own key (proof of
    /// possession); its subject and extensions are ignored.
    pub fn sign(
        &self,
        csr_pem: &str,
        node_id: &str,
        lifetime: Duration,
    ) -> Result<IssuedCert, ValkvorkError> {
        let bad = |e: String| ValkvorkError::Tls(format!("certificate request: {e}"));
        let (_, pem) =
            x509_parser::pem::parse_x509_pem(csr_pem.as_bytes()).map_err(|e| bad(e.to_string()))?;
        let (_, req) =
            X509CertificationRequest::from_der(&pem.contents).map_err(|e| bad(e.to_string()))?;
        req.verify_signature()
            .map_err(|e| bad(format!("bad signature: {e}")))?;
        let csr =
            CertificateSigningRequestParams::from_pem(csr_pem).map_err(|e| bad(e.to_string()))?;

        let now = OffsetDateTime::now_utc();
        let mut p = CertificateParams::default();
        p.distinguished_name = dn(node_id);
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        p.use_authority_key_identifier_extension = true;
        p.not_before = now - BACKDATE;
        p.not_after = now + lifetime;
        p.serial_number = Some(random_serial());
        let cert = p
            .signed_by(&csr.public_key, &self.issuer, &self.key)
            .map_err(tls_err("sign node certificate"))?;
        let info = parse_der(cert.der())?;
        Ok(IssuedCert {
            cert_pem: cert.pem(),
            serial: info.serial,
            not_after_ms: info.not_after_ms,
        })
    }
}

/// 16 random bytes, never negative and never with a leading zero byte, so
/// the DER encoding is exactly these bytes.
fn random_serial() -> SerialNumber {
    let mut b = *uuid::Uuid::new_v4().as_bytes();
    b[0] = (b[0] & 0x7f) | 0x40;
    SerialNumber::from_slice(&b)
}

/// What the controller and agent need to know about a node certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    pub common_name: String,
    /// Lower-case hex.
    pub serial: String,
    pub not_before_ms: i64,
    pub not_after_ms: i64,
}

impl CertInfo {
    /// Renew once two thirds of the lifetime has passed.
    pub fn renewal_due(&self, now_ms: i64) -> bool {
        let life = self.not_after_ms - self.not_before_ms;
        now_ms >= self.not_before_ms + life * 2 / 3
    }

    pub fn expired(&self, now_ms: i64) -> bool {
        now_ms >= self.not_after_ms
    }
}

pub fn parse_der(der: &[u8]) -> Result<CertInfo, ValkvorkError> {
    let (_, cert) = X509Certificate::from_der(der)
        .map_err(|e| ValkvorkError::Tls(format!("cannot parse certificate: {e}")))?;
    let common_name = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .unwrap_or_default()
        .to_string();
    let v = cert.validity();
    Ok(CertInfo {
        common_name,
        serial: hex::encode(cert.raw_serial()),
        not_before_ms: v.not_before.timestamp() * 1000,
        not_after_ms: v.not_after.timestamp() * 1000,
    })
}

pub fn parse_pem(pem: &str) -> Result<CertInfo, ValkvorkError> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| ValkvorkError::Tls(format!("cannot parse certificate PEM: {e}")))?;
    parse_der(&pem.contents)
}

/// Whether the certificate in `cert_pem` is for the key in `key_pem`.
pub fn cert_matches_key(cert_pem: &str, key_pem: &str) -> bool {
    let Ok(key) = KeyPair::from_pem(key_pem) else {
        return false;
    };
    let Ok((_, pem)) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()) else {
        return false;
    };
    let Ok((_, cert)) = X509Certificate::from_der(&pem.contents) else {
        return false;
    };
    cert.public_key().raw == key.public_key_der().as_slice()
}

/// A new key (P-256, PKCS#8 PEM) and a CSR for it (PEM), for the agent.
pub fn new_key_and_csr(node_id: &str) -> Result<(String, String), ValkvorkError> {
    let key = KeyPair::generate().map_err(tls_err("generate node key"))?;
    let mut p = CertificateParams::default();
    p.distinguished_name = dn(node_id);
    let csr = p
        .serialize_request(&key)
        .map_err(tls_err("certificate request"))?;
    let csr = csr.pem().map_err(tls_err("certificate request"))?;
    Ok((key.serialize_pem(), csr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::prelude::*;

    const DAY: Duration = Duration::from_secs(24 * 3600);

    #[test]
    fn signs_a_client_certificate_that_chains_to_the_stored_ca() {
        let ca = NodeCa::generate().unwrap();
        // Reload, as the controller does after a restart.
        let ca = NodeCa::from_pem(ca.cert_pem(), &ca.key_pem()).unwrap();
        let (_key, csr) = new_key_and_csr("attacker-chosen-name").unwrap();
        let issued = ca.sign(&csr, "node-1", 30 * DAY).unwrap();

        let info = parse_pem(&issued.cert_pem).unwrap();
        assert_eq!(info.common_name, "node-1", "the CSR's subject is ignored");
        assert_eq!(info.serial, issued.serial);
        assert_eq!(info.not_after_ms, issued.not_after_ms);
        assert!(!info.renewal_due(info.not_before_ms + 10 * 24 * 3600 * 1000));
        assert!(info.renewal_due(info.not_before_ms + 21 * 24 * 3600 * 1000));

        let (_, ca_pem) = parse_x509_pem(ca.cert_pem().as_bytes()).unwrap();
        let (_, ca_cert) = X509Certificate::from_der(&ca_pem.contents).unwrap();
        let (_, pem) = parse_x509_pem(issued.cert_pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(&pem.contents).unwrap();
        assert_eq!(cert.issuer().as_raw(), ca_cert.subject().as_raw());
        cert.verify_signature(Some(ca_cert.public_key())).unwrap();
        let eku = cert.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth && !eku.server_auth);
        assert!(!cert.is_ca());

        assert!(cert_matches_key(&issued.cert_pem, &_key));
        let (other, _) = new_key_and_csr("x").unwrap();
        assert!(!cert_matches_key(&issued.cert_pem, &other));
    }

    #[test]
    fn rejects_a_csr_with_a_bad_signature() {
        let ca = NodeCa::generate().unwrap();
        let (_k, csr) = new_key_and_csr("n").unwrap();
        // Flip a byte near the end of the DER (inside the signature).
        let (_, pem) = parse_x509_pem(csr.as_bytes()).unwrap();
        let mut der = pem.contents.clone();
        let i = der.len() - 3;
        der[i] ^= 0xff;
        let bad = format!(
            "-----BEGIN CERTIFICATE REQUEST-----\n{}\n-----END CERTIFICATE REQUEST-----\n",
            base64_lines(&der)
        );
        assert!(ca.sign(&bad, "n", DAY).is_err());
        assert!(ca.sign("not a csr", "n", DAY).is_err());
    }

    fn base64_lines(der: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in der.chunks(3) {
            let n = (c[0] as u32) << 16
                | (*c.get(1).unwrap_or(&0) as u32) << 8
                | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() {
                    out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}
