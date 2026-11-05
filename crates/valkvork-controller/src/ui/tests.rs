//! UI server tests against a real core and SQLite file (no network).

use super::*;
use crate::core::Core;
use crate::db::Db;
use valkvork_common::config::ControllerConfig;

const TOKEN: &str = "admin-token-0123456789abcdef0123456789";
const MANIFEST: &str = "[service]\nid = \"web\"\nimage = \"nginx:1.27\"\nreplicas = 1\n\n[resources]\ncpu = 0.5\nmemory = \"64MiB\"\n";

async fn setup() -> (Arc<UiState>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.db");
    let cfg = ControllerConfig {
        recovery_grace_period_seconds: 0,
        ..Default::default()
    };
    let core = Core::new(cfg, Db::open(&path).unwrap(), "join".into()).unwrap();
    let reader = Arc::new(EventReader::open(&path).unwrap());
    let (handle, _task) = core.spawn();
    (
        UiState::new(
            handle,
            reader,
            TOKEN.into(),
            Duration::from_secs(3600),
            false,
            path,
        ),
        dir,
    )
}

fn request(
    method: Method,
    uri: &str,
    body: Option<Value>,
    cookie: Option<&str>,
    csrf: Option<&str>,
) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    if let Some(t) = csrf {
        b = b.header(CSRF_HEADER, t);
    }
    match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    }
}

async fn json_of(r: Response<Body>) -> Value {
    let bytes = hyper::body::to_bytes(r.into_body()).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Returns (cookie header value, csrf token).
async fn login(state: &Arc<UiState>) -> (String, String) {
    let r = handle(
        state.clone(),
        request(
            Method::POST,
            "/ui/api/login",
            Some(json!({ "token": TOKEN })),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let set = r
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let cookie = set.split(';').next().unwrap().to_string();
    let csrf = json_of(r).await["csrf"].as_str().unwrap().to_string();
    (cookie, csrf)
}

#[tokio::test]
async fn pages_have_strict_csp_and_deep_links() {
    let (s, _d) = setup().await;
    let r = handle(
        s.clone(),
        request(Method::GET, "/ui/services/frontend-web", None, None, None),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::CONTENT_SECURITY_POLICY], CSP);
    assert!(r.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let r = handle(s.clone(), request(Method::GET, "/", None, None, None)).await;
    assert_eq!(r.status(), StatusCode::FOUND);
    assert_eq!(r.headers()[header::LOCATION], "/ui/");
    // Even errors carry the CSP.
    let r = handle(s, request(Method::GET, "/nope", None, None, None)).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(r.headers()[header::CONTENT_SECURITY_POLICY], CSP);
}

#[tokio::test]
async fn assets_are_gzipped_and_cacheable() {
    let (s, _d) = setup().await;
    let mut req = request(Method::GET, "/ui/assets/app.js", None, None, None);
    req.headers_mut()
        .insert(header::ACCEPT_ENCODING, "br, gzip".parse().unwrap());
    let r = handle(s.clone(), req).await;
    assert_eq!(r.headers()[header::CONTENT_ENCODING], "gzip");
    let etag = r.headers()[header::ETAG].clone();
    let mut req = request(Method::GET, "/ui/assets/app.js", None, None, None);
    req.headers_mut().insert(header::IF_NONE_MATCH, etag);
    assert_eq!(handle(s, req).await.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn api_needs_a_session() {
    let (s, _d) = setup().await;
    let r = handle(
        s.clone(),
        request(Method::GET, "/ui/api/overview", None, None, None),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/login",
            Some(json!({ "token": "wrong" })),
            None,
            None,
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = handle(
        s.clone(),
        request(
            Method::GET,
            "/ui/api/overview",
            None,
            Some("vk_session=forged"),
            None,
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn session_cookie_attributes() {
    let (s, _d) = setup().await;
    let r = handle(
        s,
        request(
            Method::POST,
            "/ui/api/login",
            Some(json!({ "token": TOKEN })),
            None,
            None,
        ),
    )
    .await;
    let set = r.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_string();
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/ui", "Max-Age=3600"] {
        assert!(set.contains(attr), "{set}");
    }
    // Secure is only set when the listener uses TLS (here it does not).
    assert!(!set.contains("Secure"));
}

#[tokio::test]
async fn mutations_need_csrf_and_are_audited() {
    let (s, _d) = setup().await;
    let (cookie, csrf) = login(&s).await;

    // AT-16: a mutating request without the CSRF header gets 403.
    let r = handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/apply",
            Some(json!({ "manifest": MANIFEST })),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/apply",
            Some(json!({ "manifest": MANIFEST })),
            Some(&cookie),
            Some("bad"),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    let r = handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/apply",
            Some(json!({ "manifest": MANIFEST })),
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(json_of(r).await["change"], "created");

    let r = handle(
        s.clone(),
        request(
            Method::GET,
            "/ui/api/cluster-events?kind=audit",
            None,
            Some(&cookie),
            None,
        ),
    )
    .await;
    let events = json_of(r).await;
    let audit = &events.as_array().unwrap()[0];
    assert_eq!(audit["kind"], "AUDIT");
    assert!(audit["actor"].as_str().unwrap().starts_with("ui:"));
    // The audit id is not the session cookie.
    assert!(!cookie.contains(audit["actor"].as_str().unwrap().trim_start_matches("ui:")));

    let r = handle(
        s.clone(),
        request(Method::GET, "/ui/api/services", None, Some(&cookie), None),
    )
    .await;
    assert_eq!(json_of(r).await[0]["service_id"], "web");
}

#[tokio::test]
async fn validation_errors_are_structured() {
    let (s, _d) = setup().await;
    let (cookie, csrf) = login(&s).await;
    let bad = MANIFEST.replace("id = \"web\"", "id = \"Bad_Id\"");
    let r = handle(
        s,
        request(
            Method::POST,
            "/ui/api/plan",
            Some(json!({ "manifest": bad })),
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = json_of(r).await;
    assert_eq!(v["errors"][0]["code"], "INVALID_SERVICE_ID");
    assert_eq!(v["errors"][0]["field"], "service.id");
}

#[tokio::test]
async fn delete_needs_the_typed_service_id() {
    let (s, _d) = setup().await;
    let (cookie, csrf) = login(&s).await;
    handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/apply",
            Some(json!({ "manifest": MANIFEST })),
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    let r = handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/services/web/delete",
            Some(json!({ "confirm": "wrong" })),
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/services/web/delete",
            Some(json!({ "confirm": "web" })),
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = handle(
        s,
        request(
            Method::POST,
            "/ui/api/nodes/missing/drain",
            None,
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn overview_and_detail_views() {
    let (s, _d) = setup().await;
    let (cookie, csrf) = login(&s).await;
    handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/apply",
            Some(json!({ "manifest": MANIFEST })),
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    let v = json_of(
        handle(
            s.clone(),
            request(Method::GET, "/ui/api/overview", None, Some(&cookie), None),
        )
        .await,
    )
    .await;
    assert_eq!(v["overview"]["services_total"], 1);
    assert_eq!(v["overview"]["unplaced_pending"], 1, "no nodes registered");
    assert!(v["controller"]["db_bytes"].as_u64().unwrap() > 0);
    assert!(!v["events"].as_array().unwrap().is_empty());
    let v = json_of(
        handle(
            s.clone(),
            request(
                Method::GET,
                "/ui/api/services/web",
                None,
                Some(&cookie),
                None,
            ),
        )
        .await,
    )
    .await;
    assert_eq!(v["service"]["service_id"], "web");
    assert!(v["cpu_series"].is_array());
    let r = handle(
        s,
        request(
            Method::GET,
            "/ui/api/services/nope",
            None,
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sse_pushes_a_change_per_commit() {
    let (s, _d) = setup().await;
    let (cookie, csrf) = login(&s).await;
    let r = handle(
        s.clone(),
        request(Method::GET, "/ui/api/events", None, Some(&cookie), None),
    )
    .await;
    assert_eq!(r.headers()[header::CONTENT_TYPE], "text/event-stream");
    let mut body = r.into_body();
    handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/apply",
            Some(json!({ "manifest": MANIFEST })),
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    let mut seen = String::new();
    while !seen.contains("event: change") {
        let chunk = tokio::time::timeout(Duration::from_secs(5), body.data())
            .await
            .expect("SSE timed out")
            .unwrap()
            .unwrap();
        seen.push_str(std::str::from_utf8(&chunk).unwrap());
    }
}

#[tokio::test]
async fn logout_ends_the_session() {
    let (s, _d) = setup().await;
    let (cookie, csrf) = login(&s).await;
    let r = handle(
        s.clone(),
        request(
            Method::POST,
            "/ui/api/logout",
            None,
            Some(&cookie),
            Some(&csrf),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = handle(
        s,
        request(Method::GET, "/ui/api/nodes", None, Some(&cookie), None),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn decodes_query_values() {
    assert_eq!(percent_decode("a%20b+c%2Fd"), "a b c/d");
    assert_eq!(percent_decode("100%"), "100%");
    assert_eq!(percent_decode("%zz"), "%zz");
}
