//! Node identity and capacity (FR-AGE-02).

use std::net::UdpSocket;
use std::path::Path;
use uuid::Uuid;
use valkvork_common::ValkvorkError;

/// Fixed namespace for `node_id = UUIDv5(namespace, machine-id)`.
const NODE_ID_NAMESPACE: Uuid = Uuid::from_u128(0x4e15_7846_1971_478f_93f8_c997_03b2_8555);

pub fn node_id_from_machine_id(machine_id: &str) -> String {
    Uuid::new_v5(&NODE_ID_NAMESPACE, machine_id.trim().as_bytes()).to_string()
}

/// Load `<data_dir>/node_id`, or derive it from `/etc/machine-id` on first
/// boot and persist it. Stable across agent restarts; distinct per cloned VM
/// because the image build truncates machine-id (FR-AGE-01, AT-14).
pub fn load_or_create_node_id(data_dir: &Path) -> Result<String, ValkvorkError> {
    let path = data_dir.join("node_id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_string();
        if Uuid::parse_str(&id).is_ok() {
            return Ok(id);
        }
    }
    let id = match std::fs::read_to_string("/etc/machine-id") {
        Ok(m) if !m.trim().is_empty() => node_id_from_machine_id(&m),
        _ => {
            tracing::warn!(
                "/etc/machine-id missing or empty; using a random node id (development only)"
            );
            Uuid::new_v4().to_string()
        }
    };
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(&path, &id)?;
    Ok(id)
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// The local address used to reach the controller. A UDP "connect" picks the
/// route without sending any packet.
pub fn local_ip_towards(endpoint: &str) -> Option<String> {
    let uri: tonic::transport::Uri = endpoint.parse().ok()?;
    let host = uri.host()?;
    let port = uri.port_u16().unwrap_or(50051);
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect((host, port)).ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_is_deterministic() {
        assert_eq!(
            node_id_from_machine_id("abc\n"),
            node_id_from_machine_id("abc")
        );
        assert_ne!(
            node_id_from_machine_id("abc"),
            node_id_from_machine_id("abd")
        );
    }

    #[test]
    fn node_id_persists() {
        let d = tempfile::tempdir().unwrap();
        let a = load_or_create_node_id(d.path()).unwrap();
        assert_eq!(a, load_or_create_node_id(d.path()).unwrap());
    }
}
