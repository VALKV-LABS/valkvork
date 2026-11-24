//! The node's client certificate (FR-SEC-02).
//!
//! The controller signs it from a CSR the agent sends over its stream. It is
//! kept with its key in one file, `<data_dir>/node.pem` (root `0600`), which
//! is replaced atomically, so a crash never leaves a certificate next to the
//! wrong key. The agent presents it as its TLS client certificate from the
//! next connection on, and asks for a new one two thirds through its
//! lifetime.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use valkvork_common::models::now_ms;
use valkvork_common::node_cert::{self, CertInfo};
use valkvork_common::ValkvorkError;

/// An unanswered request is sent again after this long.
const REQUEST_RETRY: Duration = Duration::from_secs(600);

pub struct NodeCerts {
    path: PathBuf,
    node_id: String,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The stored certificate and key (PEM), if valid for this node.
    current: Option<(String, String, CertInfo)>,
    /// The key of the outstanding request and when it was sent.
    pending: Option<(String, Instant)>,
}

impl NodeCerts {
    pub fn load(data_dir: &std::path::Path, node_id: &str) -> Self {
        let certs = Self {
            path: data_dir.join("node.pem"),
            node_id: node_id.to_string(),
            state: Mutex::new(State::default()),
        };
        match std::fs::read_to_string(&certs.path) {
            Ok(pem) => match certs.check(&pem) {
                Ok(c) => {
                    tracing::info!(
                        serial = %c.2.serial,
                        not_after_ms = c.2.not_after_ms,
                        "loaded the node's client certificate"
                    );
                    certs.state.lock().unwrap().current = Some(c);
                }
                Err(e) => {
                    tracing::warn!(path = %certs.path.display(), error = %e, "ignoring the stored client certificate")
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %certs.path.display(), error = %e, "cannot read the stored client certificate")
            }
        }
        certs
    }

    /// Split a stored `node.pem` and check it belongs to this node and key.
    fn check(&self, pem: &str) -> Result<(String, String, CertInfo), ValkvorkError> {
        let bad = |m: &str| ValkvorkError::Tls(m.to_string());
        let at = pem
            .find("-----BEGIN PRIVATE KEY-----")
            .ok_or_else(|| bad("no private key"))?;
        let (cert, key) = (pem[..at].to_string(), pem[at..].to_string());
        let info = node_cert::parse_pem(&cert)?;
        if info.common_name != self.node_id {
            return Err(bad("the certificate is for another node"));
        }
        if !node_cert::cert_matches_key(&cert, &key) {
            return Err(bad("the certificate does not match the key"));
        }
        Ok((cert, key, info))
    }

    /// The certificate and key to present, unless missing or expired.
    pub fn identity(&self) -> Option<(String, String)> {
        let st = self.state.lock().unwrap();
        st.current
            .as_ref()
            .filter(|(_, _, info)| !info.expired(now_ms()))
            .map(|(c, k, _)| (c.clone(), k.clone()))
    }

    /// A CSR to send, when the node has no usable certificate or its
    /// renewal is due, and no request is outstanding.
    pub fn request_if_due(&self) -> Option<String> {
        let mut st = self.state.lock().unwrap();
        let due = st
            .current
            .as_ref()
            .is_none_or(|(_, _, info)| info.renewal_due(now_ms()));
        let waiting = st
            .pending
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() < REQUEST_RETRY);
        if !due || waiting {
            return None;
        }
        match node_cert::new_key_and_csr(&self.node_id) {
            Ok((key, csr)) => {
                st.pending = Some((key, Instant::now()));
                Some(csr)
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot create a certificate request");
                None
            }
        }
    }

    /// Store the controller's answer to the outstanding request.
    pub fn accept(&self, cert_pem: &str) -> Result<CertInfo, ValkvorkError> {
        let mut st = self.state.lock().unwrap();
        let (key, _) = st
            .pending
            .take()
            .ok_or_else(|| ValkvorkError::Tls("no certificate request is outstanding".into()))?;
        let mut pem = cert_pem.trim_end().to_string();
        pem.push('\n');
        pem.push_str(&key);
        let checked = self.check(&pem)?;
        write_private(&self.path, &pem)?;
        let info = checked.2.clone();
        st.current = Some(checked);
        Ok(info)
    }

    /// Stop presenting the stored certificate and ask for a new one. The
    /// file stays until a new certificate replaces it.
    pub fn forget(&self) {
        self.state.lock().unwrap().current = None;
    }

    /// The outstanding request was refused; allow a new one after the
    /// retry interval rather than at once.
    pub fn refused(&self) {
        if let Some((_, at)) = self.state.lock().unwrap().pending.as_mut() {
            *at = Instant::now();
        }
    }
}

/// Write `0600` to a temporary file, then rename over `path`.
fn write_private(path: &std::path::Path, contents: &str) -> Result<(), ValkvorkError> {
    use std::io::Write;
    let tmp = path.with_extension("pem.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use valkvork_common::node_cert::NodeCa;

    const DAY: Duration = Duration::from_secs(86_400);

    #[test]
    fn requests_stores_and_reloads_a_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let ca = NodeCa::generate().unwrap();
        let certs = NodeCerts::load(dir.path(), "node-1");
        assert!(certs.identity().is_none());

        let csr = certs.request_if_due().expect("no certificate yet");
        assert!(
            certs.request_if_due().is_none(),
            "one outstanding request at a time"
        );
        let issued = ca.sign(&csr, "node-1", 30 * DAY).unwrap();
        certs.accept(&issued.cert_pem).unwrap();
        assert!(certs.identity().is_some());
        assert!(certs.request_if_due().is_none(), "not due yet");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("node.pem"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let again = NodeCerts::load(dir.path(), "node-1");
        assert_eq!(again.identity(), certs.identity());
        // Another node id (a cloned data_dir) ignores it.
        assert!(NodeCerts::load(dir.path(), "node-2").identity().is_none());
    }

    #[test]
    fn rejects_a_certificate_for_another_key_or_node() {
        let dir = tempfile::tempdir().unwrap();
        let ca = NodeCa::generate().unwrap();
        let certs = NodeCerts::load(dir.path(), "node-1");
        assert!(certs.accept("x").is_err(), "nothing outstanding");

        let _ = certs.request_if_due().unwrap();
        let (_, other_csr) = node_cert::new_key_and_csr("node-1").unwrap();
        let other = ca.sign(&other_csr, "node-1", DAY).unwrap();
        assert!(certs.accept(&other.cert_pem).is_err(), "wrong key");

        let csr = {
            certs.state.lock().unwrap().pending = None;
            certs.request_if_due().unwrap()
        };
        let wrong_node = ca.sign(&csr, "node-2", DAY).unwrap();
        assert!(certs.accept(&wrong_node.cert_pem).is_err(), "wrong node");
        assert!(certs.identity().is_none());
        assert!(!dir.path().join("node.pem").exists());
    }
}
