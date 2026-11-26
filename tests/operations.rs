#![forbid(unsafe_code)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{HeaderValue, Request, StatusCode},
    response::Response,
};
use futures_util::StreamExt;
use restreamer_core::{App, config::Environment, router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

const ROOT: &str = "operations-admin-password";
const PASSWORD: &str = "fixture-managed-password";
const NEW_PASSWORD: &str = "rotated-managed-password";

struct Node {
    app: Arc<App>,
    router: Router,
}

impl Node {
    async fn open(dir: &TempDir, env: &[(&str, &str)]) -> Self {
        let database = dir.path().join("operations.sqlite");
        let media = dir.path().join("media");
        let env = Environment::from_lookup(|name| match name {
            "CORE_DATABASE_PATH" => database.to_str().map(str::to_owned),
            "CORE_STORAGE_DISK_DIR" => media.to_str().map(str::to_owned),
            _ => env
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).into()),
        })
        .unwrap();
        let app = Arc::new(App::open(env).await.unwrap());
        let router = router(app.clone());
        Self { app, router }
    }

    async fn raw(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .extension(ConnectInfo(
                "127.0.0.1:12001".parse::<SocketAddr>().unwrap(),
            ));
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        self.router
            .clone()
            .oneshot(
                request
                    .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        code: u16,
    ) -> Value {
        let response = self.raw(method, path, token, body).await;
        let status = response.status();
        let data = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(
            status,
            code,
            "{method} {path}: {}",
            String::from_utf8_lossy(&data)
        );
        serde_json::from_slice(&data).unwrap()
    }

    async fn login(&self, name: &str, password: &str) -> (String, String) {
        let data = self
            .call(
                "POST",
                "/api/login",
                None,
                Some(json!({"username":name,"password":password})),
                200,
            )
            .await;
        (
            data["access_token"].as_str().unwrap().into(),
            data["refresh_token"].as_str().unwrap().into(),
        )
    }
}

fn job(id: &str, zone: &str, cap: u64) -> Value {
    json!({"id": id, "domain":"tenant", "zone":zone,
        "input":[{"id":"in","address":"testsrc2=size=64x64:rate=10","options":["-f","lavfi"]}],
        "output":[{"id":"out","address":"-","options":["-f","null"]}],
        "limits":{"memory_mbytes":cap}
    })
}

#[tokio::test]
async fn identities_scope_access_and_revoke_tokens_across_restarts() {
    let dir = TempDir::new().unwrap();
    let node = Node::open(&dir, &[("CORE_API_AUTH_PASSWORD", ROOT)]).await;
    let (root, _) = node.login("admin", ROOT).await;
    for id in ["visible", "private"] {
        node.call(
            "POST",
            "/api/v3/process",
            Some(&root),
            Some(job(id, "$default", 0)),
            200,
        )
        .await;
    }
    let policies = json!([
        {"domain":"tenant","types":["process"],"resource":"*","actions":["read"]},
        {"domain":"tenant","types":["process"],"resource":"private","actions":["read"],"effect":"deny"}
    ]);
    let account = node.call("POST", "/api/v3/iam/user", Some(&root), Some(json!({
        "name":"operator","alias":"Operator","auth":{"api":{"userpass":PASSWORD}},"policies":policies
    })), 200).await;
    assert_eq!(account["auth"]["api"]["userpass"], "");
    assert!(!account.to_string().contains(PASSWORD));
    let (access, refresh) = node.login("operator", PASSWORD).await;
    let visible = node
        .call(
            "GET",
            "/api/v3/process?domain=tenant&filter=config",
            Some(&access),
            None,
            200,
        )
        .await;
    assert_eq!(visible.as_array().unwrap().len(), 1);
    assert_eq!(visible[0]["id"], "visible");
    node.call(
        "GET",
        "/api/v3/process/private?domain=tenant",
        Some(&access),
        None,
        403,
    )
    .await;
    node.call("GET", "/api/v3/process/visible", Some(&access), None, 403)
        .await;
    node.call(
        "GET",
        "/api/v3/process/visible?domain=tenant",
        Some(&access),
        None,
        200,
    )
    .await;
    node.call(
        "DELETE",
        "/api/v3/process/visible?domain=tenant",
        Some(&access),
        None,
        403,
    )
    .await;
    node.call("GET", "/api/v3/config", Some(&access), None, 403)
        .await;
    node.call("GET", "/api/login/refresh", Some(&refresh), None, 200)
        .await;
    node.call("GET", "/api/login/refresh", Some(&access), None, 403)
        .await;
    node.call("DELETE", "/api/v3/iam/user/admin", Some(&root), None, 403)
        .await;
    node.call(
        "PUT",
        "/api/v3/iam/user/operator/policy",
        Some(&root),
        Some(json!([])),
        200,
    )
    .await;
    node.call(
        "GET",
        "/api/v3/process/visible?domain=tenant",
        Some(&access),
        None,
        403,
    )
    .await;
    node.call(
        "PUT",
        "/api/v3/iam/user/operator",
        Some(&root),
        Some(json!({"auth":{"api":{"userpass":NEW_PASSWORD}}})),
        200,
    )
    .await;
    node.call("GET", "/api/login/refresh", Some(&refresh), None, 401)
        .await;
    node.app.close().await;
    let node = Node::open(&dir, &[]).await;
    let (root, _) = node.login("admin", ROOT).await;
    let (access, _) = node.login("operator", NEW_PASSWORD).await;
    node.call(
        "PUT",
        "/api/v3/iam/user/operator/revoke",
        Some(&root),
        None,
        200,
    )
    .await;
    node.call("GET", "/api", Some(&access), None, 401).await;
    let (access, _) = node.login("operator", NEW_PASSWORD).await;
    node.call(
        "PUT",
        "/api/v3/iam/user/operator",
        Some(&root),
        Some(json!({"active":false})),
        200,
    )
    .await;
    node.call("GET", "/api", Some(&access), None, 401).await;
    let events = node
        .call("GET", "/api/v3/audit", Some(&root), None, 200)
        .await;
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["action"] == "revoke")
    );
    assert!(!events.to_string().contains(NEW_PASSWORD));
    node.call(
        "PUT",
        "/api/v3/iam/signing-key/rotate",
        Some(&root),
        None,
        200,
    )
    .await;
    node.call("GET", "/api", Some(&root), None, 401).await;
    node.login("admin", ROOT).await;
    node.app.close().await;
}

#[tokio::test]
async fn zone_reservations_are_transactional_and_release_on_delete() {
    let dir = TempDir::new().unwrap();
    let node = Node::open(&dir, &[]).await;
    node.call(
        "POST",
        "/api/v3/zone",
        None,
        Some(json!({"id":"production","limits":{"memory_mbytes":16}})),
        200,
    )
    .await;
    let a = node.call(
        "POST",
        "/api/v3/process",
        None,
        Some(job("first", "production", 12)),
        200,
    );
    let b = node.call(
        "POST",
        "/api/v3/process",
        None,
        Some(job("second", "production", 12)),
        409,
    );
    a.await;
    b.await;
    let zone = node
        .call("GET", "/api/v3/zone/production", None, None, 200)
        .await;
    assert_eq!(zone["usage"]["memory_mbytes"], 12);
    assert_eq!(zone["processes"], 1);
    node.call("DELETE", "/api/v3/zone/production", None, None, 409)
        .await;
    node.call("DELETE", "/api/v3/zone/$default", None, None, 400)
        .await;
    node.call(
        "DELETE",
        "/api/v3/process/first?domain=tenant",
        None,
        None,
        200,
    )
    .await;
    node.call(
        "POST",
        "/api/v3/process",
        None,
        Some(job("second", "production", 12)),
        200,
    )
    .await;
    node.call(
        "POST",
        "/api/v3/process",
        None,
        Some(job("third", "missing", 0)),
        400,
    )
    .await;
    node.app.close().await;
    let node = Node::open(&dir, &[]).await;
    let zone = node
        .call("GET", "/api/v3/zone/production", None, None, 200)
        .await;
    assert_eq!(zone["usage"]["memory_mbytes"], 12);
    node.app.close().await;
}

#[tokio::test]
async fn readiness_metrics_logs_and_events_are_bounded() {
    let dir = TempDir::new().unwrap();
    let node = Node::open(
        &dir,
        &[
            ("CORE_METRICS_ENABLE", "true"),
            ("CORE_METRICS_ENABLE_PROMETHEUS", "true"),
            ("CORE_LOG_MAX_LINES", "4"),
        ],
    )
    .await;
    node.call("GET", "/healthz", None, None, 200).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if node.raw("GET", "/readyz", None, None).await.status() == StatusCode::OK {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let descriptions = node.call("GET", "/api/v3/metrics", None, None, 200).await;
    assert!(
        descriptions
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "mem_total")
    );
    let scrape = node.raw("GET", "/metrics", None, None).await;
    assert_eq!(scrape.status(), 200);
    let scrape = to_bytes(scrape.into_body(), 128 * 1024).await.unwrap();
    let scrape = std::str::from_utf8(&scrape).unwrap();
    assert!(scrape.contains("castvane_http_requests_total"));
    assert!(!scrape.contains("127.0.0.1"));
    let mut stream = node
        .raw(
            "POST",
            "/api/v3/events/log",
            None,
            Some(json!({"filters":[{"event":"configuration"}]})),
        )
        .await
        .into_body()
        .into_data_stream();
    let frame = stream.next().await.unwrap().unwrap();
    assert!(frame.starts_with(b":keepalive"));
    node.call(
        "PUT",
        "/api/v3/config",
        None,
        Some(json!({"version":3,"name":"ops"})),
        200,
    )
    .await;
    let frame = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(frame.starts_with(b"event: configuration"));
    for _ in 0..10 {
        let subscriber = tracing_subscriber::registry().with(node.app.capture_logs());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target:"restreamer_core::test","Authorization: Bearer do-not-return-this");
        });
    }
    let log = node
        .call("GET", "/api/v3/log?format=raw", None, None, 200)
        .await;
    assert_eq!(log.as_array().unwrap().len(), 4);
    assert!(!log.to_string().contains("do-not-return-this"));
    drop(stream);
    node.app.close().await;
}

#[tokio::test]
async fn trusted_proxy_cannot_trigger_a_socket_loopback_bypass_without_a_peer() {
    let dir = TempDir::new().unwrap();
    let node = Node::open(
        &dir,
        &[
            ("CORE_API_AUTH_PASSWORD", ROOT),
            ("CORE_API_AUTH_DISABLE_LOCALHOST", "true"),
            ("CORE_API_ACCESS_TRUSTED_PROXIES", "127.0.0.1/32"),
        ],
    )
    .await;
    node.call("GET", "/api/v3/config", None, None, 403).await;
    let request = Request::builder()
        .uri("/api/v3/config")
        .header(
            "X-Forwarded-For",
            HeaderValue::from_static("127.0.0.1, 192.0.2.1"),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12001".parse::<SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        node.router.clone().oneshot(request).await.unwrap().status(),
        403
    );
    node.app.close().await;
}

#[tokio::test]
async fn last_administrator_and_optional_transport_do_not_hide_startup_failures() {
    let dir = TempDir::new().unwrap();
    let node = Node::open(
        &dir,
        &[
            ("CORE_RTMP_ENABLE", "true"),
            (
                "CORE_MEDIA_TRANSPORT_BINARY",
                "/this-fixture-does-not-exist/mediamtx",
            ),
        ],
    )
    .await;
    node.call(
        "POST",
        "/api/v3/iam/user",
        None,
        Some(json!({
            "name":"operator","superuser":true,"auth":{"api":{"userpass":PASSWORD}}
        })),
        200,
    )
    .await;
    node.call(
        "PUT",
        "/api/v3/iam/user/operator",
        None,
        Some(json!({"active":false})),
        403,
    )
    .await;
    node.call("DELETE", "/api/v3/iam/user/operator", None, None, 403)
        .await;
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = node.raw("GET", "/readyz", None, None).await;
            if response.status().is_success() {
                break serde_json::from_slice::<Value>(
                    &to_bytes(response.into_body(), 16384).await.unwrap(),
                )
                .unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(snapshot["native_media"], false);
    assert_eq!(snapshot["status"], "ok");
    node.app.close().await;
}

fn certificate(cert: &std::path::Path, key: &std::path::Path) {
    let result = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-noenc",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-out",
        ])
        .arg(cert)
        .arg("-keyout")
        .arg(key)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(result.success());
}

#[tokio::test]
async fn tls_rejects_mismatched_keys_and_reloads_a_complete_replacement() {
    use axum::serve::Listener;
    let dir = TempDir::new().unwrap();
    let chain = dir.path().join("chain.pem");
    let key = dir.path().join("private.pem");
    let next_chain = dir.path().join("next-chain.pem");
    let next_key = dir.path().join("next-private.pem");
    certificate(&chain, &key);
    certificate(&next_chain, &next_key);
    let failed_dir = TempDir::new().unwrap();
    let failed = Node::open(
        &failed_dir,
        &[
            ("CORE_TLS_ENABLE", "true"),
            ("CORE_TLS_ADDRESS", "127.0.0.1:0"),
            ("CORE_TLS_CERT_FILE", chain.to_str().unwrap()),
            ("CORE_TLS_KEY_FILE", next_key.to_str().unwrap()),
        ],
    )
    .await;
    assert!(failed.app.bind_tls().await.is_err());
    failed.app.close().await;
    let node = Node::open(
        &dir,
        &[
            ("CORE_TLS_ENABLE", "true"),
            ("CORE_TLS_ADDRESS", "127.0.0.1:0"),
            ("CORE_TLS_CERT_FILE", chain.to_str().unwrap()),
            ("CORE_TLS_KEY_FILE", key.to_str().unwrap()),
        ],
    )
    .await;
    let listener = node.app.bind_tls().await.unwrap().unwrap();
    let url = format!("https://{}/ping", listener.local_addr().unwrap().0);
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(node.app.clone().serve(listener, stop.clone()));
    let first = reqwest::Certificate::from_pem(&std::fs::read(&chain).unwrap()).unwrap();
    let second = reqwest::Certificate::from_pem(&std::fs::read(&next_chain).unwrap()).unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(first)
        .build()
        .unwrap();
    assert_eq!(
        client.get(&url).send().await.unwrap().text().await.unwrap(),
        "pong"
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(second)
        .build()
        .unwrap();
    std::fs::rename(next_chain, chain).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::rename(next_key, key).unwrap();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if let Ok(response) = client.get(&url).send().await
                && response.status().is_success()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .unwrap();
    stop.cancel();
    task.await.unwrap();
    node.app.close().await;
}

#[tokio::test]
#[ignore = "repeatable operations benchmark"]
async fn benchmark_parallel_management_and_metrics() {
    let dir = TempDir::new().unwrap();
    let node = Arc::new(
        Node::open(
            &dir,
            &[
                ("CORE_API_AUTH_PASSWORD", ROOT),
                ("CORE_METRICS_ENABLE", "true"),
                ("CORE_METRICS_ENABLE_PROMETHEUS", "true"),
            ],
        )
        .await,
    );
    let start = std::time::Instant::now();
    let (root, _) = node.login("admin", ROOT).await;
    println!(
        "Argon2id login including router, database and JWT: {:.3} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    node.call("POST","/api/v3/iam/user",Some(&root),Some(json!({
        "name":"viewer","auth":{"api":{"userpass":PASSWORD}},
        "policies":[{"domain":"$none","types":["api"],"resource":"/metrics","actions":["read"]},
                    {"domain":"$none","types":["api"],"resource":"/api/v3/config","actions":["read"]}]
    })),200).await;
    let (user, _) = node.login("viewer", PASSWORD).await;
    for path in ["/api/v3/config", "/metrics"] {
        let start = std::time::Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let node = node.clone();
            let user = user.clone();
            tasks.spawn(async move {
                for _ in 0..32 {
                    let response = node.raw("GET", path, Some(&user), None).await;
                    assert_eq!(response.status(), 200);
                    to_bytes(response.into_body(), 128 * 1024).await.unwrap();
                }
            });
        }
        while let Some(task) = tasks.join_next().await {
            task.unwrap();
        }
        println!(
            "{path} with eight clients: {:.0} requests/s",
            256.0 / start.elapsed().as_secs_f64()
        );
    }
    let upload = Request::builder()
        .method("PUT")
        .uri("/api/v3/fs/mem/benchmark.m4s")
        .header("authorization", format!("Bearer {root}"))
        .extension(ConnectInfo(
            "127.0.0.1:12001".parse::<SocketAddr>().unwrap(),
        ))
        .body(Body::from(vec![3; 1024 * 1024]))
        .unwrap();
    assert_eq!(
        node.router.clone().oneshot(upload).await.unwrap().status(),
        201
    );
    for with_scraping in [false, true] {
        let start = std::time::Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        if with_scraping {
            let node = node.clone();
            let user = user.clone();
            tasks.spawn(async move {
                for _ in 0..128 {
                    let response = node.raw("GET", "/metrics", Some(&user), None).await;
                    assert_eq!(response.status(), 200);
                }
            });
        }
        for _ in 0..4 {
            let node = node.clone();
            tasks.spawn(async move {
                for _ in 0..16 {
                    let response = node.raw("GET", "/memfs/benchmark.m4s", None, None).await;
                    assert_eq!(response.status(), 200);
                    assert_eq!(
                        to_bytes(response.into_body(), 2 * 1024 * 1024)
                            .await
                            .unwrap()
                            .len(),
                        1024 * 1024
                    );
                }
            });
        }
        while let Some(task) = tasks.join_next().await {
            task.unwrap();
        }
        println!(
            "media bodies with concurrent scrapes={with_scraping}: {:.1} MiB/s",
            64.0 / start.elapsed().as_secs_f64()
        );
    }
    let start = std::time::Instant::now();
    let subscriber = tracing_subscriber::registry().with(node.app.capture_logs());
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..10_000 {
            tracing::info!(target: "restreamer_core::bench", "bounded log fixture");
        }
    });
    println!(
        "log capture with bounded event dispatch: {:.0} events/s",
        10000.0 / start.elapsed().as_secs_f64()
    );
    node.app.close().await;
}
