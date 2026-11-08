//! Resolution of workloads' secret references (FR-SEC-04).
//!
//! The controller stores and sends only references; the agent resolves them
//! just before it creates a container and passes the values as environment
//! variables. Values are never logged, stored or reported back. Errors name
//! the reference and the cause, never a value.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::OnceCell;
use valkvork_common::config::SecretsConfig;

/// The EC2 instance metadata service.
const IMDS: &str = "169.254.169.254:80";

pub struct SecretResolver {
    cfg: SecretsConfig,
    imds: String,
    /// `cfg.aws_region`, or the instance's region from the metadata service,
    /// looked up once.
    region: OnceCell<Option<String>>,
    /// ECR registry -> (password, fetched at). Tokens last 12 h; reused for 6.
    ecr: Mutex<HashMap<String, (String, Instant)>>,
}

/// How long an ECR login is reused (they are valid for 12 hours).
const ECR_REUSE: Duration = Duration::from_secs(6 * 3600);

/// The region of an Amazon ECR registry host
/// (`<account>.dkr.ecr.<region>.amazonaws.com`), or None for any other.
pub fn ecr_region(registry: &str) -> Option<&str> {
    let mut parts = registry.split('.');
    let (account, dkr, ecr, region) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let rest: Vec<&str> = parts.collect();
    let ok = account.len() == 12
        && account.bytes().all(|b| b.is_ascii_digit())
        && dkr == "dkr"
        && ecr == "ecr"
        && !region.is_empty()
        && (rest == ["amazonaws", "com"] || rest == ["amazonaws", "com", "cn"]);
    ok.then_some(region)
}

impl SecretResolver {
    pub fn new(cfg: SecretsConfig) -> Self {
        Self {
            cfg,
            imds: IMDS.to_string(),
            region: OnceCell::new(),
            ecr: Mutex::new(HashMap::new()),
        }
    }

    /// A Docker login password for an ECR registry, from
    /// `aws ecr get-login-password` with the node's role (needs
    /// ecr:GetAuthorizationToken), reused for 6 hours.
    pub async fn ecr_password(&self, registry: &str, region: &str) -> Result<String, String> {
        if let Some((p, at)) = self.ecr.lock().unwrap().get(registry) {
            if at.elapsed() < ECR_REUSE {
                return Ok(p.clone());
            }
        }
        let password = self
            .aws(&["ecr", "get-login-password", "--region", region], None)
            .await?;
        self.ecr
            .lock()
            .unwrap()
            .insert(registry.to_string(), (password.clone(), Instant::now()));
        tracing::info!(registry, "ECR login from the node's role");
        Ok(password)
    }

    /// The region for ssm: references. Unset in agent.toml: this instance's
    /// region (IMDSv2), as on the golden image, else the CLI's own default.
    async fn region(&self) -> Option<String> {
        self.region
            .get_or_init(|| async {
                if let Some(r) = &self.cfg.aws_region {
                    return Some(r.clone());
                }
                let found = imds_region(&self.imds).await;
                match &found {
                    Some(r) => {
                        tracing::info!(region = %r, "secrets: region from instance metadata")
                    }
                    None => tracing::info!(
                        "secrets: no instance metadata; the AWS CLI chooses the region"
                    ),
                }
                found
            })
            .await
            .clone()
    }

    /// The value behind `reference`.
    pub async fn resolve(&self, reference: &str) -> Result<String, String> {
        if let Some(param) = reference.strip_prefix("ssm:") {
            self.ssm(param).await
        } else if let Some(path) = reference.strip_prefix("file:") {
            self.file(Path::new(path))
        } else {
            Err("unknown reference scheme".into())
        }
    }

    fn file(&self, path: &Path) -> Result<String, String> {
        if self.cfg.file_dirs.is_empty() {
            return Err(
                "file: references are disabled on this node (agent.toml secrets.file_dirs)".into(),
            );
        }
        // Resolve symlinks and "..": the file itself must be inside an allowed directory.
        let real = path.canonicalize().map_err(|e| e.to_string())?;
        let allowed = self
            .cfg
            .file_dirs
            .iter()
            .filter_map(|d| d.canonicalize().ok())
            .any(|d| real.starts_with(&d));
        if !allowed {
            return Err("not inside an allowed directory (agent.toml secrets.file_dirs)".into());
        }
        let text = std::fs::read_to_string(&real).map_err(|e| e.to_string())?;
        Ok(strip_newline(text))
    }

    async fn ssm(&self, param: &str) -> Result<String, String> {
        let region = self.region().await;
        self.aws(
            &[
                "ssm",
                "get-parameter",
                "--with-decryption",
                "--name",
                param,
                "--query",
                "Parameter.Value",
                "--output",
                "text",
            ],
            region.as_deref(),
        )
        .await
    }

    /// Run the AWS CLI; its output with one trailing newline dropped.
    async fn aws(&self, args: &[&str], region: Option<&str>) -> Result<String, String> {
        let mut cmd = tokio::process::Command::new(&self.cfg.aws_cli);
        cmd.args(args);
        if let Some(region) = region {
            cmd.args(["--region", region]);
        }
        cmd.env("AWS_PAGER", "")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let run = cmd.output();
        let out = tokio::time::timeout(Duration::from_secs(self.cfg.timeout_seconds), run)
            .await
            .map_err(|_| format!("timed out after {}s", self.cfg.timeout_seconds))?
            .map_err(|e| format!("cannot run {}: {e}", self.cfg.aws_cli.display()))?;
        if !out.status.success() {
            return Err(last_line(&out.stderr));
        }
        let value = String::from_utf8(out.stdout).map_err(|_| "value is not UTF-8".to_string())?;
        Ok(strip_newline(value))
    }
}

/// This instance's region from IMDSv2, or None off EC2 (2 s limit).
async fn imds_region(addr: &str) -> Option<String> {
    let attempt = async {
        let token = http(
            addr,
            "PUT",
            "/latest/api/token",
            "X-aws-ec2-metadata-token-ttl-seconds: 60",
        )
        .await?;
        let region = http(
            addr,
            "GET",
            "/latest/meta-data/placement/region",
            &format!("X-aws-ec2-metadata-token: {token}"),
        )
        .await?;
        let region = region.trim().to_string();
        let valid = !region.is_empty()
            && region.len() < 32
            && region
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        valid.then_some(region)
    };
    tokio::time::timeout(Duration::from_secs(2), attempt)
        .await
        .ok()
        .flatten()
}

/// A minimal HTTP/1.1 request to the metadata service: the body of a 200.
async fn http(addr: &str, method: &str, path: &str, header: &str) -> Option<String> {
    let mut conn = tokio::net::TcpStream::connect(addr).await.ok()?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 169.254.169.254\r\n{header}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    conn.write_all(req.as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    conn.take(16 * 1024).read_to_end(&mut buf).await.ok()?;
    let text = String::from_utf8(buf).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    head.split_whitespace().nth(1).filter(|s| *s == "200")?;
    Some(body.to_string())
}

/// One trailing newline is formatting, not part of the value.
fn strip_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

/// The CLI's error, e.g. "An error occurred (ParameterNotFound) when calling
/// the GetParameter operation: ...", shortened.
fn last_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("failed without an error message");
    line.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn resolver(dir: &Path, file_dirs: Vec<PathBuf>) -> SecretResolver {
        // A stand-in for the AWS CLI: prints the parameter's value or fails
        // like the real one, and records its arguments.
        let cli = dir.join("aws");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\necho \"$@\" > {log}\nfor a; do last=$a; case $prev in --name) name=$a;; esac; prev=$a; done\n\
                 case $name in\n  /ok) printf 's3cr3t\\n';;\n  /multi) printf 'line1\\nline2\\n';;\n  /slow) sleep 5;;\n  \
                 *) echo 'An error occurred (ParameterNotFound) when calling the GetParameter operation: Parameter not found.' >&2; exit 254;;\nesac\n",
                log = dir.join("args").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        SecretResolver::new(SecretsConfig {
            aws_cli: cli,
            aws_region: Some("us-west-1".into()),
            file_dirs,
            timeout_seconds: 1,
        })
    }

    #[tokio::test]
    async fn ssm_values_errors_and_timeouts() {
        let d = tempfile::tempdir().unwrap();
        let r = resolver(d.path(), vec![]);
        assert_eq!(r.resolve("ssm:/ok").await.unwrap(), "s3cr3t");
        let args = std::fs::read_to_string(d.path().join("args")).unwrap();
        assert!(
            args.contains("--with-decryption") && args.contains("--region us-west-1"),
            "{args}"
        );
        assert_eq!(r.resolve("ssm:/multi").await.unwrap(), "line1\nline2");
        let e = r.resolve("ssm:/missing").await.unwrap_err();
        assert!(e.contains("ParameterNotFound"), "{e}");
        assert!(r
            .resolve("ssm:/slow")
            .await
            .unwrap_err()
            .contains("timed out"));
        assert!(r.resolve("vault:x").await.is_err());
    }

    /// Without aws_region, the region comes from the metadata service once.
    #[tokio::test]
    async fn region_from_instance_metadata() {
        use tokio::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if req.starts_with("PUT /latest/api/token") {
                    "tok123"
                } else if req.starts_with("GET /latest/meta-data/placement/region")
                    && req.contains("X-aws-ec2-metadata-token: tok123")
                {
                    "us-west-1"
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
        let d = tempfile::tempdir().unwrap();
        let mut r = resolver(d.path(), vec![]);
        r.cfg.aws_region = None;
        r.imds = addr;
        assert_eq!(r.resolve("ssm:/ok").await.unwrap(), "s3cr3t");
        let args = std::fs::read_to_string(d.path().join("args")).unwrap();
        assert!(args.contains("--region us-west-1"), "{args}");

        // Off EC2: no metadata service, no --region.
        let mut off = resolver(d.path(), vec![]);
        off.cfg.aws_region = None;
        off.imds = "127.0.0.1:1".into();
        assert_eq!(off.resolve("ssm:/ok").await.unwrap(), "s3cr3t");
        let args = std::fs::read_to_string(d.path().join("args")).unwrap();
        assert!(!args.contains("--region"), "{args}");
    }

    #[tokio::test]
    async fn files_only_from_allowed_directories() {
        let d = tempfile::tempdir().unwrap();
        let allowed = d.path().join("secrets");
        std::fs::create_dir(&allowed).unwrap();
        std::fs::write(allowed.join("pw"), "hunter2\n").unwrap();
        std::fs::write(d.path().join("outside"), "nope").unwrap();
        std::os::unix::fs::symlink(d.path().join("outside"), allowed.join("link")).unwrap();

        let disabled = resolver(d.path(), vec![]);
        let pw = format!("file:{}", allowed.join("pw").display());
        assert!(disabled
            .resolve(&pw)
            .await
            .unwrap_err()
            .contains("disabled"));

        let r = resolver(d.path(), vec![allowed.clone()]);
        assert_eq!(r.resolve(&pw).await.unwrap(), "hunter2");
        for bad in [
            d.path().join("outside"),
            allowed.join("link"),
            allowed.join("../outside"),
        ] {
            let e = r
                .resolve(&format!("file:{}", bad.display()))
                .await
                .unwrap_err();
            assert!(e.contains("allowed directory"), "{}: {e}", bad.display());
        }
    }

    #[test]
    fn ecr_registries() {
        assert_eq!(
            ecr_region("936356193095.dkr.ecr.us-west-1.amazonaws.com"),
            Some("us-west-1")
        );
        assert_eq!(ecr_region("docker.io"), None);
        assert_eq!(ecr_region("ghcr.io"), None);
        assert_eq!(ecr_region("1234.dkr.ecr.us-west-1.amazonaws.com"), None);
        assert_eq!(ecr_region("936356193095.dkr.ecr.us-west-1.evil.com"), None);
    }

    /// The ECR password comes from the CLI once and is then reused.
    #[tokio::test]
    async fn ecr_login_is_cached() {
        let d = tempfile::tempdir().unwrap();
        let cli = d.path().join("aws");
        let count = d.path().join("count");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\necho \"$@\" >> {count}\n[ \"$1 $2\" = \"ecr get-login-password\" ] && printf 'tok3n\\n' && exit 0\nexit 1\n",
                count = count.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let r = SecretResolver::new(SecretsConfig {
            aws_cli: cli,
            aws_region: None,
            file_dirs: vec![],
            timeout_seconds: 2,
        });
        let reg = "936356193095.dkr.ecr.us-west-1.amazonaws.com";
        assert_eq!(r.ecr_password(reg, "us-west-1").await.unwrap(), "tok3n");
        assert_eq!(r.ecr_password(reg, "us-west-1").await.unwrap(), "tok3n");
        let calls = std::fs::read_to_string(&count).unwrap();
        assert_eq!(calls.lines().count(), 1, "{calls}");
        assert!(calls.contains("--region us-west-1"));
    }
}
