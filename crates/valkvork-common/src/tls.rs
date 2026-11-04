//! TLS and token helpers built on tonic's rustls integration.

use crate::errors::ValkvorkError;
use std::path::Path;
use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};

/// Server TLS from PEM certificate chain and private key files.
pub fn server_tls_config(cert: &Path, key: &Path) -> Result<ServerTlsConfig, ValkvorkError> {
    let cert = read(cert)?;
    let key = read(key)?;
    Ok(ServerTlsConfig::new().identity(Identity::from_pem(cert, key)))
}

/// Client TLS that trusts only the CA bundle at `ca` (FR-SEC-01), or the
/// platform roots when `ca` is `None`.
pub fn client_tls_config(
    ca: Option<&Path>,
    domain: Option<&str>,
) -> Result<ClientTlsConfig, ValkvorkError> {
    let mut cfg = ClientTlsConfig::new();
    if let Some(ca) = ca {
        cfg = cfg.ca_certificate(Certificate::from_pem(read(ca)?));
    }
    if let Some(domain) = domain {
        cfg = cfg.domain_name(domain);
    }
    Ok(cfg)
}

fn read(path: &Path) -> Result<Vec<u8>, ValkvorkError> {
    std::fs::read(path)
        .map_err(|e| ValkvorkError::Tls(format!("cannot read {}: {e}", path.display())))
}

/// Constant-time token comparison, so response timing does not leak how much
/// of a guessed token was correct.
pub fn tokens_equal(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_compare() {
        assert!(super::tokens_equal("abc", "abc"));
        assert!(!super::tokens_equal("abc", "abd"));
        assert!(!super::tokens_equal("abc", "ab"));
    }
}
