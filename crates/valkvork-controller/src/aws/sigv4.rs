//! AWS Signature Version 4, for the EC2 Query API
//! (https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv.html).

use ring::{digest, hmac};

pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// One request to sign. Headers are (lower-case name, value), and must
/// include `host` and `x-amz-date`.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
        .as_ref()
        .to_vec()
}

/// The `Authorization` header for `req`, at `amz_date` (`YYYYMMDDTHHMMSSZ`).
pub fn authorization(
    creds: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
    req: &Request<'_>,
) -> String {
    let date = &amz_date[..8];
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        req.path,
        req.query,
        canonical_headers,
        signed_headers,
        sha256_hex(req.body)
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let k = hmac(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    let k = hmac(&k, b"aws4_request");
    let signature = hex::encode(hmac(&k, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    )
}

/// RFC 3986 percent-encoding, as SigV4 and the Query API expect.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `k=v&...`, sorted by key, encoded.
pub fn form(params: &[(String, String)]) -> String {
    let mut p: Vec<_> = params
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    p.sort();
    p.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's SigV4 test suite, "get-vanilla".
    #[test]
    fn matches_the_aws_test_vector() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let auth = authorization(
            &creds,
            "us-east-1",
            "service",
            "20150830T123600Z",
            &Request {
                method: "GET",
                path: "/",
                query: "",
                headers: &[
                    ("host", "example.amazonaws.com"),
                    ("x-amz-date", "20150830T123600Z"),
                ],
                body: b"",
            },
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn encodes_forms() {
        assert_eq!(uri_encode("a b/c=d~"), "a%20b%2Fc%3Dd~");
        let f = form(&[
            ("Version".into(), "2016-11-15".into()),
            ("Action".into(), "DescribeVolumes".into()),
        ]);
        assert_eq!(f, "Action=DescribeVolumes&Version=2016-11-15");
    }
}
