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

/// The node's zone and instance id: agent.toml's, else the EC2 metadata
/// service's, else empty (off a cloud). Anything not a plain cloud name is
/// dropped.
pub async fn topology(zone: Option<&str>, instance_id: Option<&str>) -> (String, String) {
    topology_from(crate::secrets::IMDS, zone, instance_id).await
}

pub(crate) async fn topology_from(
    imds: &str,
    zone: Option<&str>,
    instance_id: Option<&str>,
) -> (String, String) {
    let get = |set: Option<&str>, path: &'static str| {
        let set = set.map(str::to_string);
        async move {
            match set {
                Some(v) => Some(v),
                None => crate::secrets::imds_get(imds, path).await,
            }
            .filter(|v| valkvork_common::models::is_cloud_name(v))
            .unwrap_or_default()
        }
    };
    tokio::join!(
        get(zone, "placement/availability-zone"),
        get(instance_id, "instance-id")
    )
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

    #[tokio::test]
    async fn topology_from_config_or_metadata() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if req.starts_with("PUT /latest/api/token") {
                    "tok"
                } else if req.starts_with("GET /latest/meta-data/placement/availability-zone") {
                    "us-west-1b\n"
                } else if req.starts_with("GET /latest/meta-data/instance-id") {
                    "i-0abc123"
                } else {
                    ""
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        assert_eq!(
            topology_from(&addr, None, None).await,
            ("us-west-1b".to_string(), "i-0abc123".to_string())
        );
        assert_eq!(
            topology_from(&addr, Some("local"), Some("bad id!")).await,
            ("local".to_string(), String::new())
        );
        // Off EC2: empty, quickly.
        assert_eq!(
            topology_from("127.0.0.1:9", None, None).await,
            (String::new(), String::new())
        );
    }

    #[test]
    fn node_id_persists() {
        let d = tempfile::tempdir().unwrap();
        let a = load_or_create_node_id(d.path()).unwrap();
        assert_eq!(a, load_or_create_node_id(d.path()).unwrap());
    }
}
