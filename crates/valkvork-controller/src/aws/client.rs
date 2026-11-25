//! A minimal client for AWS Query APIs (EC2): signed POSTs over HTTPS, with
//! credentials from the environment or the instance role (IMDSv2).
//!
//! Small on purpose (docs/block-volumes.md, "Decisions"): the controller
//! stays a static binary with no certificate store (the root CAs are
//! compiled in) and no AWS SDK.

use super::sigv4::{self, Credentials};
use hyper::body::HttpBody;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

const IMDS: &str = "169.254.169.254:80";
const EC2_VERSION: &str = "2016-11-15";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsError {
    /// The API's error code (`InvalidVolume.NotFound`), or a transport one.
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for AwsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

fn transport(e: impl std::fmt::Display) -> AwsError {
    AwsError {
        code: "Transport".into(),
        message: e.to_string(),
    }
}

/// Where requests go: `https://ec2.<region>.amazonaws.com`, or a test
/// server.
#[derive(Debug, Clone)]
struct Endpoint {
    tls: bool,
    host: String,
    port: u16,
}

impl Endpoint {
    fn parse(url: &str) -> Result<Self, AwsError> {
        let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(transport(format!(
                "endpoint {url:?} must be http(s)://host"
            )));
        };
        let rest = rest.trim_end_matches('/');
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(transport)?),
            None => (rest.to_string(), if tls { 443 } else { 80 }),
        };
        Ok(Self { tls, host, port })
    }
}

pub struct AwsClient {
    region: String,
    endpoint: Endpoint,
    tls: Arc<rustls::ClientConfig>,
    imds: String,
    /// Cached credentials and when to fetch new ones.
    creds: Mutex<Option<(Arc<Credentials>, Instant)>>,
}

impl AwsClient {
    /// A client for EC2 in `region` (None: this instance's, from IMDS).
    /// `endpoint` overrides `https://ec2.<region>.amazonaws.com`.
    pub async fn new(region: Option<String>, endpoint: Option<String>) -> Result<Self, AwsError> {
        let region = match region {
            Some(r) => r,
            None => imds_get(IMDS, "/latest/meta-data/placement/region")
                .await
                .ok_or_else(|| {
                    transport("no region configured, and none from instance metadata")
                })?,
        };
        let url = endpoint.unwrap_or_else(|| format!("https://ec2.{region}.amazonaws.com"));
        let mut roots = rustls::RootCertStore::empty();
        roots.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
            rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));
        let tls = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            region,
            endpoint: Endpoint::parse(&url)?,
            tls: Arc::new(tls),
            imds: IMDS.to_string(),
            creds: Mutex::new(None),
        })
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    /// For tests: fixed credentials, never refreshed.
    pub async fn set_credentials(&self, c: Credentials) {
        *self.creds.lock().await =
            Some((Arc::new(c), Instant::now() + Duration::from_secs(86_400)));
    }

    async fn credentials(&self) -> Result<Arc<Credentials>, AwsError> {
        let mut cached = self.creds.lock().await;
        if let Some((c, until)) = cached.as_ref() {
            if Instant::now() < *until {
                return Ok(c.clone());
            }
        }
        let (c, valid_for) = if let (Ok(id), Ok(secret)) = (
            std::env::var("AWS_ACCESS_KEY_ID"),
            std::env::var("AWS_SECRET_ACCESS_KEY"),
        ) {
            let c = Credentials {
                access_key_id: id,
                secret_access_key: secret,
                session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
            };
            (c, Duration::from_secs(300))
        } else {
            instance_role_credentials(&self.imds).await?
        };
        let c = Arc::new(c);
        *cached = Some((c.clone(), Instant::now() + valid_for));
        Ok(c)
    }

    /// One EC2 Query API call; the response body (XML) on HTTP 200.
    pub async fn ec2(&self, action: &str, params: &[(String, String)]) -> Result<String, AwsError> {
        let mut all: Vec<(String, String)> = vec![
            ("Action".into(), action.into()),
            ("Version".into(), EC2_VERSION.into()),
        ];
        all.extend(params.iter().cloned());
        let body = sigv4::form(&all);
        let creds = self.credentials().await?;
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let host = if self.endpoint.port == 443 || self.endpoint.port == 80 {
            self.endpoint.host.clone()
        } else {
            format!("{}:{}", self.endpoint.host, self.endpoint.port)
        };
        let ctype = "application/x-www-form-urlencoded; charset=utf-8";
        let mut headers = vec![
            ("content-type", ctype),
            ("host", host.as_str()),
            ("x-amz-date", amz_date.as_str()),
        ];
        if let Some(t) = &creds.session_token {
            headers.push(("x-amz-security-token", t.as_str()));
        }
        let auth = sigv4::authorization(
            &creds,
            &self.region,
            "ec2",
            &amz_date,
            &sigv4::Request {
                method: "POST",
                path: "/",
                query: "",
                headers: &headers,
                body: body.as_bytes(),
            },
        );
        let mut req = http::Request::builder().method("POST").uri("/");
        for (k, v) in &headers {
            req = req.header(*k, *v);
        }
        let req = req
            .header("authorization", auth)
            .body(hyper::Body::from(body))
            .map_err(transport)?;
        let (status, text) = tokio::time::timeout(Duration::from_secs(30), self.send(req))
            .await
            .map_err(|_| transport(format!("{action}: no answer within 30 s")))??;
        if status == 200 {
            return Ok(text);
        }
        Err(AwsError {
            code: super::xml::text(&text, "Code").unwrap_or_else(|| format!("HTTP {status}")),
            message: super::xml::text(&text, "Message")
                .unwrap_or_else(|| text.chars().take(300).collect()),
        })
    }

    async fn send(&self, req: http::Request<hyper::Body>) -> Result<(u16, String), AwsError> {
        let tcp = tokio::net::TcpStream::connect((self.endpoint.host.as_str(), self.endpoint.port))
            .await
            .map_err(transport)?;
        if self.endpoint.tls {
            let name =
                rustls::ServerName::try_from(self.endpoint.host.as_str()).map_err(transport)?;
            let tls = tokio_rustls::TlsConnector::from(self.tls.clone())
                .connect(name, tcp)
                .await
                .map_err(transport)?;
            exchange(tls, req).await
        } else {
            exchange(tcp, req).await
        }
    }
}

async fn exchange<S>(io: S, req: http::Request<hyper::Body>) -> Result<(u16, String), AwsError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::handshake(io)
        .await
        .map_err(transport)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender.send_request(req).await.map_err(transport)?;
    let status = resp.status().as_u16();
    let mut body = resp.into_body();
    let mut buf = Vec::new();
    while let Some(chunk) = body.data().await {
        buf.extend_from_slice(&chunk.map_err(transport)?);
        if buf.len() > 8 << 20 {
            return Err(transport("response over 8 MiB"));
        }
    }
    Ok((status, String::from_utf8_lossy(&buf).into_owned()))
}

/// The instance role's credentials (IMDSv2) and how long to keep them:
/// until five minutes before they expire.
async fn instance_role_credentials(imds: &str) -> Result<(Credentials, Duration), AwsError> {
    let none = || {
        transport("no AWS credentials: no AWS_ACCESS_KEY_ID, and no instance role in the metadata service")
    };
    let base = "/latest/meta-data/iam/security-credentials/";
    let role = imds_get(imds, base).await.ok_or_else(none)?;
    let role = role.lines().next().unwrap_or("").trim().to_string();
    if role.is_empty() {
        return Err(none());
    }
    let doc = imds_get(imds, &format!("{base}{role}"))
        .await
        .ok_or_else(none)?;
    let v: serde_json::Value = serde_json::from_str(&doc).map_err(transport)?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let c = Credentials {
        access_key_id: s("AccessKeyId").ok_or_else(none)?,
        secret_access_key: s("SecretAccessKey").ok_or_else(none)?,
        session_token: s("Token"),
    };
    let valid_for = s("Expiration")
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(&e).ok())
        .map(|exp| (exp.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds() - 300)
        .filter(|s| *s > 0)
        .map(|s| Duration::from_secs(s as u64))
        .unwrap_or(Duration::from_secs(60));
    Ok((c, valid_for))
}

/// One IMDSv2 GET: the body of a 200, or None (2 s limit).
pub(crate) async fn imds_get(addr: &str, path: &str) -> Option<String> {
    async fn raw(addr: &str, method: &str, path: &str, header: &str) -> Option<String> {
        let mut conn = tokio::net::TcpStream::connect(addr).await.ok()?;
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 169.254.169.254\r\n{header}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        conn.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        conn.take(64 * 1024).read_to_end(&mut buf).await.ok()?;
        let text = String::from_utf8(buf).ok()?;
        let (head, body) = text.split_once("\r\n\r\n")?;
        head.split_whitespace().nth(1).filter(|s| *s == "200")?;
        Some(body.to_string())
    }
    let attempt = async {
        let token = raw(
            addr,
            "PUT",
            "/latest/api/token",
            "X-aws-ec2-metadata-token-ttl-seconds: 300",
        )
        .await?;
        raw(
            addr,
            "GET",
            path,
            &format!("X-aws-ec2-metadata-token: {token}"),
        )
        .await
        .map(|b| b.trim().to_string())
    };
    tokio::time::timeout(Duration::from_secs(2), attempt)
        .await
        .ok()
        .flatten()
}
