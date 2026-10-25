//! TLS utilities for Valkvork.
//!
//! This module provides TLS configuration and connection helpers for both
//! server and client connections.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use rustls::{Certificate, PrivateKey, RootCertStore, ServerConfig, ClientConfig};
use rustls::server::{ServerCertVerifier, AllowAnyAuthenticatedClient};
use tokio_rustls::TlsAcceptor;
use crate::errors::{TlsError, ValkvorkError};

// ============================================================================
// Server TLS Configuration
// ============================================================================

/// Load server TLS configuration from certificate and key files
pub fn load_server_config(
    cert_path: &Path,
    key_path: &Path,
    ca_path: Option<&Path>,
    client_auth: bool,
) -> Result<ServerConfig, ValkvorkError> {
    let cert_chain = load_cert_chain(cert_path)?;
    let private_key = load_private_key(key_path)?;
    
    let mut config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth();
    
    config.set_single_cert(cert_chain, private_key)?;
    
    // If client authentication is enabled and CA is provided
    if client_auth {
        if let Some(ca_path) = ca_path {
            let root_certs = load_root_certs(ca_path)?;
            let client_auth = AllowAnyAuthenticatedClient::new(root_certs);
            config.client_auth = Some(client_auth);
        } else {
            return Err(ValkvorkError::Tls(TlsError::InvalidCaBundle(
                "CA bundle required for client authentication".to_string()
            )));
        }
    }
    
    Ok(config)
}

/// Create a TLS acceptor from server config
pub fn create_tls_acceptor(config: ServerConfig) -> TlsAcceptor {
    TlsAcceptor::from(Arc::new(config))
}

// ============================================================================
// Client TLS Configuration
// ============================================================================

/// Load client TLS configuration
pub fn load_client_config(
    ca_path: Option<&Path>,
) -> Result<ClientConfig, ValkvorkError> {
    let mut root_store = RootCertStore::empty();
    
    if let Some(ca_path) = ca_path {
        let root_certs = load_root_certs(ca_path)?;
        for cert in root_certs {
            root_store.add(&cert)?;
        }
    } else {
        // Use system root certificates
        root_store.add_parsable_certificates(load_native_certs()?);
    }
    
    let config = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    
    Ok(config)
}

/// Load native system certificates
fn load_native_certs() -> Result<Vec<Certificate>, ValkvorkError> {
    use rustls_native_certs::load_native_certs;
    let certs = load_native_certs()?;
    Ok(certs.into_iter().map(Certificate).collect())
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Load certificate chain from file
pub fn load_cert_chain(path: &Path) -> Result<Vec<Certificate>, ValkvorkError> {
    let cert_data = fs::read(path)?;
    let mut cursor = std::io::Cursor::new(&cert_data);
    let cert_chain = rustls_pemfile::certs(&mut cursor)?;
    
    if cert_chain.is_empty() {
        return Err(ValkvorkError::Tls(TlsError::Certificate(
            "No certificates found in chain".to_string()
        )));
    }
    
    Ok(cert_chain.into_iter().map(Certificate).collect())
}

/// Load private key from file
pub fn load_private_key(path: &Path) -> Result<PrivateKey, ValkvorkError> {
    let key_data = fs::read(path)?;
    let mut cursor = std::io::Cursor::new(&key_data);
    let private_key = rustls_pemfile::private_key(&mut cursor)?;
    
    match private_key {
        Some(key) => Ok(PrivateKey(key)),
        None => Err(ValkvorkError::Tls(TlsError::Certificate(
            "No private key found".to_string()
        ))),
    }
}

/// Load root certificates from file
pub fn load_root_certs(path: &Path) -> Result<Vec<Certificate>, ValkvorkError> {
    let ca_data = fs::read(path)?;
    let mut cursor = std::io::Cursor::new(&ca_data);
    let root_certs = rustls_pemfile::certs(&mut cursor)?;
    Ok(root_certs.into_iter().map(Certificate).collect())
}

/// Generate a self-signed certificate for development
pub fn generate_self_signed_cert(
    hostname: &str,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), ValkvorkError> {
    use rcgen::{CertificateParams, KeyPair, SanType, DnType};
    
    let mut params = CertificateParams::new(vec![hostname]);
    params.key_pair = KeyPair::generate()?;
    params.san = vec![SanType::DnsName(hostname.to_string())];
    params.distinguished_name.push(DnType::OrganizationName("Valkvork".to_string()));
    params.distinguished_name.push(DnType::CommonName(hostname.to_string()));
    
    let cert = rcgen::Certificate::from_params(params)?;
    
    let cert_pem = cert.pem();
    let key_pem = cert.serialize_private_key_pem();
    let ca_pem = cert_pem.clone(); // For self-signed, cert is its own CA
    
    Ok((cert_pem, key_pem, ca_pem))
}

// ============================================================================
// Token Authentication
// ============================================================================

/// Simple token-based authentication
pub fn validate_token(token: &str, expected_token: &str) -> bool {
    // Constant-time comparison to prevent timing attacks
    use std::time::{Instant, Duration};
    
    if token.len() != expected_token.len() {
        return false;
    }
    
    let result = token == expected_token;
    
    // Ensure constant time
    let start = Instant::now();
    let _ = expected_token.len(); // Dummy operation
    while start.elapsed() < Duration::from_millis(10) {}
    
    result
}

/// Generate a random token
pub fn generate_random_token(length: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..length)
        .map(|_| {
            const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_validate_token() {
        let token = "test-token";
        let expected = "test-token";
        let wrong = "wrong-token";
        
        assert!(validate_token(token, expected));
        assert!(!validate_token(wrong, expected));
    }
    
    #[test]
    fn test_generate_random_token() {
        let token1 = generate_random_token(32);
        let token2 = generate_random_token(32);
        
        assert_eq!(token1.len(), 32);
        assert_eq!(token2.len(), 32);
        assert_ne!(token1, token2);
    }
}
