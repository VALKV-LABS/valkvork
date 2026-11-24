//! Containers' access to the cloud instance metadata service.
//!
//! Every container on a node can otherwise reach 169.254.169.254 and use the
//! node's role, so it can read every secret the node can. With agent.toml
//! `[instance_metadata] block`, the agent keeps one iptables chain, jumped
//! to from Docker's `DOCKER-USER`, that rejects traffic from the Docker
//! bridge to the metadata address, except from the addresses of running
//! containers labelled [`LABEL_INSTANCE_METADATA`] (manifests with
//! `[security] instance_metadata = true`).
//!
//! The chain is rewritten whole with `iptables-restore --noflush`, which is
//! atomic, from the containers Docker reports, so it is right again after
//! every sync whatever happened in between (an agent restart, a missed
//! event, a Docker restart).

use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Containers that may reach the metadata service carry this label, "true".
pub const LABEL_INSTANCE_METADATA: &str = "valkvork.instance_metadata";
pub const CHAIN: &str = "VALKVORK-IMDS";
const METADATA_ADDR: &str = "169.254.169.254/32";

pub struct MetadataGuard {
    iptables: String,
    bridge: String,
    /// Serialises syncs, so an older view never overwrites a newer one.
    pub(crate) lock: tokio::sync::Mutex<Option<Vec<String>>>,
}

impl MetadataGuard {
    /// Check that iptables works and that Docker's `DOCKER-USER` chain
    /// exists, and create the chain and its jump. A chain left by an earlier
    /// run is kept until the first sync, so running containers that may
    /// reach the service are not cut off while the agent starts.
    pub async fn install(iptables: &str, bridge: &str) -> Result<Self, String> {
        let g = Self {
            iptables: iptables.to_string(),
            bridge: bridge.to_string(),
            lock: tokio::sync::Mutex::new(None),
        };
        g.run(&["-w", "-L", "DOCKER-USER", "-n"]).await.map_err(|e| {
            format!(
                "instance_metadata.block needs Docker's DOCKER-USER iptables chain ({e}); \
                 is Docker managing iptables? Set [instance_metadata] block = false to run without it"
            )
        })?;
        if g.run(&["-w", "-L", CHAIN, "-n"]).await.is_err() {
            // New chain: block everything until the first sync.
            g.restore(&rules(&[], bridge)).await?;
        }
        g.ensure_jump().await?;
        Ok(g)
    }

    /// Allow exactly `allowed` (container addresses) through. Re-checks the
    /// jump every time, since a Docker or firewall restart can drop it.
    pub async fn apply(&self, allowed: &[String]) -> Result<(), String> {
        self.ensure_jump().await?;
        self.restore(&rules(allowed, &self.bridge)).await
    }

    async fn ensure_jump(&self) -> Result<(), String> {
        let jump = ["DOCKER-USER", "-j", CHAIN];
        if self.run(&[&["-w", "-C"][..], &jump].concat()).await.is_ok() {
            return Ok(());
        }
        self.run(&["-w", "-I", "DOCKER-USER", "1", "-j", CHAIN])
            .await
            .map_err(|e| format!("cannot add the {CHAIN} jump to DOCKER-USER: {e}"))
    }

    async fn run(&self, args: &[&str]) -> Result<(), String> {
        let out = Command::new(&self.iptables)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("{}: {e}", self.iptables))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    async fn restore(&self, rules: &str) -> Result<(), String> {
        let cmd = format!("{}-restore", self.iptables);
        let mut child = Command::new(&cmd)
            .args(["-w", "--noflush"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{cmd}: {e}"))?;
        let mut stdin = child.stdin.take().expect("piped");
        stdin
            .write_all(rules.as_bytes())
            .await
            .map_err(|e| format!("{cmd}: {e}"))?;
        drop(stdin);
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| format!("{cmd}: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{cmd}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

/// The chain, as `iptables-restore` input. Declaring the chain flushes only
/// it; `--noflush` keeps the rest of the table.
pub fn rules(allowed: &[String], bridge: &str) -> String {
    let mut r = format!("*filter\n:{CHAIN} - [0:0]\n");
    for ip in allowed {
        r.push_str(&format!(
            "-A {CHAIN} -s {ip}/32 -d {METADATA_ADDR} -j RETURN\n"
        ));
    }
    r.push_str(&format!(
        "-A {CHAIN} -i {bridge} -d {METADATA_ADDR} -j REJECT\nCOMMIT\n"
    ));
    r
}

#[cfg(test)]
mod tests {
    #[test]
    fn rules_allow_listed_containers_and_reject_the_rest() {
        assert_eq!(
            super::rules(&["172.17.0.3".into(), "172.17.0.9".into()], "docker0"),
            "*filter\n\
             :VALKVORK-IMDS - [0:0]\n\
             -A VALKVORK-IMDS -s 172.17.0.3/32 -d 169.254.169.254/32 -j RETURN\n\
             -A VALKVORK-IMDS -s 172.17.0.9/32 -d 169.254.169.254/32 -j RETURN\n\
             -A VALKVORK-IMDS -i docker0 -d 169.254.169.254/32 -j REJECT\n\
             COMMIT\n"
        );
        assert!(super::rules(&[], "docker0").contains("-i docker0 -d 169.254.169.254/32 -j REJECT"));
    }
}
