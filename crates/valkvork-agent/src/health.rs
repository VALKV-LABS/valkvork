//! Health checks: the agent probes a workload through one of its published
//! ports, or runs a command inside its container, and restarts it after
//! repeated failures (see the supervisor).

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckKind {
    /// Connects to `target`.
    Tcp { target: SocketAddr },
    /// GET `path` on `target`; healthy on 2xx and 3xx.
    Http { target: SocketAddr, path: String },
    /// Runs `command` in the container (`docker exec`); healthy on exit 0.
    /// The supervisor runs it, since it needs the runtime.
    Exec { command: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub kind: CheckKind,
    pub interval: Duration,
    pub timeout: Duration,
    pub failures: u32,
    pub grace: Duration,
}

impl Probe {
    /// Container label form, so an adopted container keeps its check:
    /// `kind|target|interval|timeout|failures|grace|rest`, where `rest` is
    /// the HTTP path, or the exec command as a JSON array (target `-`).
    pub fn to_label(&self) -> String {
        let (kind, target, rest) = match &self.kind {
            CheckKind::Tcp { target } => ("tcp", target.to_string(), String::new()),
            CheckKind::Http { target, path } => ("http", target.to_string(), path.clone()),
            CheckKind::Exec { command } => (
                "exec",
                "-".to_string(),
                serde_json::to_string(command).unwrap_or_default(),
            ),
        };
        format!(
            "{kind}|{target}|{}|{}|{}|{}|{rest}",
            self.interval.as_secs(),
            self.timeout.as_secs(),
            self.failures,
            self.grace.as_secs(),
        )
    }

    pub fn from_label(s: &str) -> Option<Self> {
        let mut it = s.splitn(7, '|');
        let kind = it.next()?;
        let target = it.next()?;
        let secs = |v: Option<&str>| v?.parse::<u64>().ok().map(Duration::from_secs);
        let interval = secs(it.next())?;
        let timeout = secs(it.next())?;
        let failures = it.next()?.parse().ok()?;
        let grace = secs(it.next())?;
        let rest = it.next().unwrap_or("");
        let kind = match kind {
            "tcp" => CheckKind::Tcp {
                target: target.parse().ok()?,
            },
            "http" => CheckKind::Http {
                target: target.parse().ok()?,
                path: rest.to_string(),
            },
            "exec" => CheckKind::Exec {
                command: serde_json::from_str(rest)
                    .ok()
                    .filter(|c: &Vec<String>| !c.is_empty())?,
            },
            _ => return None,
        };
        Some(Self {
            kind,
            interval,
            timeout,
            failures,
            grace,
        })
    }

    /// One network probe, bounded by the timeout. Exec checks are run by the
    /// supervisor, which has the runtime.
    pub async fn run(&self) -> Result<(), String> {
        tokio::time::timeout(self.timeout, self.attempt())
            .await
            .map_err(|_| format!("timed out after {}s", self.timeout.as_secs()))?
    }

    async fn attempt(&self) -> Result<(), String> {
        let (target, path) = match &self.kind {
            CheckKind::Tcp { target } => (target, None),
            CheckKind::Http { target, path } => (target, Some(path)),
            CheckKind::Exec { .. } => return Err("an exec check needs the runtime".into()),
        };
        let mut conn = TcpStream::connect(target)
            .await
            .map_err(|e| format!("connect {target}: {e}"))?;
        let Some(path) = path else {
            return Ok(());
        };
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {target}\r\nUser-Agent: valkvork-agent\r\nConnection: close\r\n\r\n"
        );
        conn.write_all(req.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        // Only the status line matters.
        let mut buf = [0u8; 256];
        let mut len = 0;
        while len < buf.len() {
            let n = conn
                .read(&mut buf[len..])
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 || buf[..len + n].contains(&b'\n') {
                len += n;
                break;
            }
            len += n;
        }
        let line = String::from_utf8_lossy(&buf[..len]);
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| "no HTTP status line".to_string())?;
        if (200..400).contains(&status) {
            Ok(())
        } else {
            Err(format!("HTTP {status}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn probe(http: bool, target: SocketAddr) -> Probe {
        Probe {
            kind: if http {
                CheckKind::Http {
                    target,
                    path: "/healthz".into(),
                }
            } else {
                CheckKind::Tcp { target }
            },
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(1),
            failures: 2,
            grace: Duration::from_secs(0),
        }
    }

    #[test]
    fn label_round_trips() {
        let p = probe(true, "10.0.0.5:8080".parse().unwrap());
        assert_eq!(Probe::from_label(&p.to_label()), Some(p));
        let tcp = probe(false, "10.0.0.5:5432".parse().unwrap());
        assert_eq!(Probe::from_label(&tcp.to_label()), Some(tcp.clone()));
        // Labels written by earlier agents still parse.
        assert_eq!(
            Probe::from_label("tcp|10.0.0.5:5432|10|2|3|0|"),
            Some(Probe {
                kind: CheckKind::Tcp {
                    target: "10.0.0.5:5432".parse().unwrap()
                },
                interval: Duration::from_secs(10),
                timeout: Duration::from_secs(2),
                failures: 3,
                grace: Duration::from_secs(0),
            })
        );
        let exec = Probe {
            kind: CheckKind::Exec {
                command: vec!["pg_isready".into(), "-U".into(), "a|b".into()],
            },
            ..tcp
        };
        assert_eq!(
            exec.to_label(),
            r#"exec|-|1|1|2|0|["pg_isready","-U","a|b"]"#
        );
        assert_eq!(Probe::from_label(&exec.to_label()), Some(exec));
        assert_eq!(Probe::from_label("exec|-|1|1|2|0|[]"), None);
        assert_eq!(Probe::from_label("udp|1.2.3.4:1|1|1|1|0|"), None);
    }

    async fn serve(status: &'static str) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn tcp_and_http() {
        let ok = serve("200 OK").await;
        let redirect = serve("302 Found").await;
        let broken = serve("503 Service Unavailable").await;
        assert!(probe(false, ok).run().await.is_ok());
        assert!(probe(true, ok).run().await.is_ok());
        assert!(probe(true, redirect).run().await.is_ok());
        assert_eq!(probe(true, broken).run().await.unwrap_err(), "HTTP 503");
        // Nothing listening: port 1 refuses (a just-released ephemeral port
        // could be reused by a parallel test).
        let closed: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(probe(false, closed)
            .run()
            .await
            .unwrap_err()
            .starts_with("connect"));
    }
}
