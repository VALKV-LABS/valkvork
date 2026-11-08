//! Health checks: the agent probes a workload through one of its published
//! ports and restarts it after repeated failures (see the supervisor).

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub http: bool,
    pub target: SocketAddr,
    pub path: String,
    pub interval: Duration,
    pub timeout: Duration,
    pub failures: u32,
    pub grace: Duration,
}

impl Probe {
    /// Container label form, so an adopted container keeps its check:
    /// `kind|target|interval|timeout|failures|grace|path`.
    pub fn to_label(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}",
            if self.http { "http" } else { "tcp" },
            self.target,
            self.interval.as_secs(),
            self.timeout.as_secs(),
            self.failures,
            self.grace.as_secs(),
            self.path
        )
    }

    pub fn from_label(s: &str) -> Option<Self> {
        let mut it = s.splitn(7, '|');
        let http = match it.next()? {
            "http" => true,
            "tcp" => false,
            _ => return None,
        };
        let target = it.next()?.parse().ok()?;
        let secs = |v: Option<&str>| v?.parse::<u64>().ok().map(Duration::from_secs);
        let interval = secs(it.next())?;
        let timeout = secs(it.next())?;
        let failures = it.next()?.parse().ok()?;
        let grace = secs(it.next())?;
        let path = it.next().unwrap_or("").to_string();
        Some(Self {
            http,
            target,
            path,
            interval,
            timeout,
            failures,
            grace,
        })
    }

    /// One probe, bounded by the timeout.
    pub async fn run(&self) -> Result<(), String> {
        tokio::time::timeout(self.timeout, self.attempt())
            .await
            .map_err(|_| format!("timed out after {}s", self.timeout.as_secs()))?
    }

    async fn attempt(&self) -> Result<(), String> {
        let mut conn = TcpStream::connect(self.target)
            .await
            .map_err(|e| format!("connect {}: {e}", self.target))?;
        if !self.http {
            return Ok(());
        }
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: valkvork-agent\r\nConnection: close\r\n\r\n",
            self.path, self.target
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
            http,
            target,
            path: "/healthz".into(),
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
        // Nothing listening.
        let closed = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        assert!(probe(false, closed)
            .run()
            .await
            .unwrap_err()
            .starts_with("connect"));
    }
}
