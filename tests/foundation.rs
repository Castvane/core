#![forbid(unsafe_code)]

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use restreamer_core::{App, config::Environment, router};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tempfile::TempDir;
use tower::ServiceExt;

const PASSWORD: &str = "fixture-password-1234";

struct Instance {
    app: Arc<App>,
    router: Router,
}

impl Instance {
    async fn open(dir: &TempDir, values: &[(&str, &str)]) -> Self {
        let path = dir.path().join("foundation.sqlite3");
        let environment = Environment::from_lookup(|name| {
            if name == "CORE_DATABASE_PATH" {
                return path.to_str().map(str::to_owned);
            }
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
        .unwrap();
        let app = Arc::new(App::open(environment).await.unwrap());
        let router = router(app.clone());
        Self { app, router }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        call(
            &self.router,
            method,
            path,
            body.map(|value| value.to_string()),
            token,
            "127.0.0.1:12345",
        )
        .await
    }

    async fn login(&self, password: &str) -> (StatusCode, Value) {
        self.request(
            "POST",
            "/api/login",
            Some(json!({"username":"admin","password":password})),
            None,
        )
        .await
    }
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<String>,
    token: Option<&str>,
    peer: &str,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, Body::from))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(uuid::Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).is_ok());
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status();
    let content_type = response.headers()["content-type"].clone();
    let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let value = if content_type
        .to_str()
        .unwrap()
        .starts_with("application/json")
    {
        serde_json::from_slice(&bytes).unwrap()
    } else {
        Value::String(String::from_utf8(bytes.to_vec()).unwrap())
    };
    (status, value)
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/foundation.json")).unwrap()
}

#[tokio::test]
async fn discovery_local_login_refresh_and_policy() {
    let dir = TempDir::new().unwrap();
    let server = Instance::open(&dir, &[("CORE_API_AUTH_PASSWORD", PASSWORD)]).await;
    let fixture = fixture();
    assert_eq!(
        server.request("GET", "/ping", None, None).await,
        (StatusCode::OK, json!("pong"))
    );
    let (status, about) = server.request("GET", "/api", None, None).await;
    assert_eq!(status, 200);
    assert_eq!(about["app"], "datarhei-core");
    assert_eq!(about["auths"], fixture["authentication_methods"]);
    assert!(about.get("id").is_none());
    assert_eq!(about["capabilities"]["media"], true);
    assert_eq!(
        server.request("GET", "/api/v3/config", None, None).await,
        (StatusCode::FORBIDDEN, fixture["access_denied"].clone())
    );
    assert_eq!(
        server.login("incorrect").await,
        (StatusCode::FORBIDDEN, fixture["bad_credentials"].clone())
    );
    let (status, tokens) = server.login(PASSWORD).await;
    assert_eq!(status, 200);
    let access = tokens["access_token"].as_str().unwrap();
    let refresh = tokens["refresh_token"].as_str().unwrap();
    for (token, key) in [(access, "access_claims"), (refresh, "refresh_claims")] {
        let claims = jsonwebtoken::dangerous::insecure_decode::<Value>(token)
            .unwrap()
            .claims;
        let required = fixture[key].as_object().unwrap();
        for (field, value) in required {
            assert_eq!(&claims[field], value);
        }
        assert_eq!(
            claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
            claims["exi"].as_i64().unwrap()
        );
        assert_eq!(claims["sub"], "admin");
        assert!(uuid::Uuid::parse_str(claims["jti"].as_str().unwrap()).is_ok());
    }

    let (status, about) = server.request("GET", "/api", None, Some(access)).await;
    assert_eq!(status, 200);
    assert_eq!(about["version"]["number"], env!("CARGO_PKG_VERSION"));
    assert!(about["id"].is_string());
    assert_eq!(
        server
            .request("GET", "/api/v3/config", None, Some("invalid"))
            .await
            .0,
        401
    );
    assert_eq!(
        server
            .request("GET", "/api/login/refresh", None, Some(access))
            .await,
        (StatusCode::FORBIDDEN, fixture["wrong_token_use"].clone())
    );
    assert_eq!(
        server
            .request("GET", "/api/v3/config", None, Some(refresh))
            .await
            .0,
        403
    );
    let (status, refreshed) = server
        .request("GET", "/api/login/refresh", None, Some(refresh))
        .await;
    assert_eq!(status, 200);
    assert_eq!(refreshed.as_object().unwrap().len(), 1);
    assert_ne!(refreshed["access_token"], tokens["access_token"]);
    let (status, docs) = server
        .request("GET", "/api/swagger/doc.json", None, Some(access))
        .await;
    assert_eq!(status, 200);
    assert_eq!(docs["openapi"], "3.1.0");
    assert!(docs["paths"].get("/api/v3/process").is_some());
    assert_eq!(
        server
            .request("GET", "/api/v3/process", None, Some(access))
            .await
            .0,
        200
    );
    assert_eq!(
        server
            .request("GET", "/api/v4/config", None, Some(access))
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn partial_updates_are_atomic_and_configuration_survives_restart() {
    let dir = TempDir::new().unwrap();
    let server = Instance::open(&dir, &[("CORE_NAME", "environment name")]).await;
    let (_, config) = server.request("GET", "/api/v3/config", None, None).await;
    assert_eq!(config["config"]["version"], 3);
    assert_eq!(config["config"]["name"], "environment name");
    assert_eq!(config["overrides"], json!(["name"]));
    let keys = config["config"]
        .as_object()
        .unwrap()
        .keys()
        .collect::<Vec<_>>();
    assert_eq!(json!(keys), fixture()["configuration_fields"]);
    let first = server.request(
        "PUT",
        "/api/v3/config",
        Some(
            json!({"version":1,"name":"persisted","rtmp":{"enable_tls":true,"address":":1936"},
            "storage":{"disk":{"cache":{"types":[".ts"]}}}}),
        ),
        None,
    );
    let second = server.request("PUT", "/api/v3/config",
        Some(json!({"version":3,"log":{"level":"debug"},"api":{"auth":{"enable":true,"username":"admin","password":PASSWORD}}})), None);
    // Exercise concurrent unrelated writes before authentication policy changes.
    assert_eq!(first.await, (StatusCode::OK, json!("OK")));
    let a = server.request(
        "PUT",
        "/api/v3/config",
        Some(json!({"version":2,"log":{"max_lines":200}})),
        None,
    );
    let b = server.request(
        "PUT",
        "/api/v3/config",
        Some(json!({"version":3,"playout":{"max_port":4444}})),
        None,
    );
    let (a, b) = tokio::join!(a, b);
    assert_eq!(a.0, 200);
    assert_eq!(b.0, 200);
    let invalid = server
        .request(
            "PUT",
            "/api/v3/config",
            Some(json!({"version":3,"name":"rejected","address":":bad"})),
            None,
        )
        .await;
    assert_eq!(
        invalid,
        (StatusCode::CONFLICT, json!({"address":["invalid value"]}))
    );
    assert_eq!(
        server
            .request(
                "PUT",
                "/api/v3/config",
                Some(json!({"version":3,"name":5})),
                None
            )
            .await
            .0,
        400
    );
    assert_eq!(
        server
            .request(
                "PUT",
                "/api/v3/config",
                Some(json!({"name":"no version"})),
                None
            )
            .await
            .0,
        400
    );
    assert_eq!(second.await.0, 200);
    let (status, tokens) = server.login(PASSWORD).await;
    assert_eq!(status, 200);
    let access = tokens["access_token"].as_str().unwrap();
    let (_, before) = server
        .request("GET", "/api/v3/config", None, Some(access))
        .await;
    assert_eq!(
        server
            .request("GET", "/api/v3/config/reload", None, Some(access))
            .await,
        (StatusCode::OK, json!("OK"))
    );
    let (_, after) = server
        .request("GET", "/api/v3/config", None, Some(access))
        .await;
    assert_ne!(before["loaded_at"], after["loaded_at"]);
    server.app.close().await;
    drop(server);
    let server = Instance::open(&dir, &[]).await;
    let (status, current) = server
        .request("GET", "/api/v3/config", None, Some(access))
        .await;
    assert_eq!(status, 200);
    let stored = &current["config"];
    assert_eq!(stored["id"], config["config"]["id"]);
    assert_eq!(stored["name"], "persisted");
    assert_eq!(stored["log"]["level"], "debug");
    assert_eq!(stored["log"]["max_lines"], 200);
    assert_eq!(stored["playout"]["max_port"], 4444);
    assert_eq!(stored["rtmp"]["address_tls"], ":1936");
    assert_eq!(
        stored["storage"]["disk"]["cache"]["types"]["allow"],
        json!([".ts"])
    );
    assert_eq!(stored["api"]["auth"]["password"], "");
    assert_eq!(stored["api"]["auth"]["jwt"]["secret"], "");
    let new_password = "rotated-password-4321";
    assert_eq!(
        server
            .request(
                "PUT",
                "/api/v3/config",
                Some(json!({"version":3,"api":{"auth":{"password":new_password}}})),
                Some(access)
            )
            .await
            .0,
        200
    );
    assert_eq!(
        server
            .request("GET", "/api/v3/config", None, Some(access))
            .await
            .0,
        401
    );
    assert_eq!(server.login(new_password).await.0, 200);
}

#[tokio::test]
async fn startup_migrates_existing_data_and_database_contains_no_plain_password() {
    let dir = TempDir::new().unwrap();
    let server = Instance::open(&dir, &[("CORE_API_AUTH_PASSWORD", PASSWORD)]).await;
    server.app.close().await;
    drop(server);
    let url = format!("sqlite:{}", dir.path().join("foundation.sqlite3").display());
    let pool = SqlitePool::connect(&url).await.unwrap();
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM administrator")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    let document: String = sqlx::query_scalar("SELECT document FROM configuration")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!document.contains(PASSWORD));
    sqlx::query("UPDATE configuration SET schema_version = 1, document = ?")
        .bind(
            json!({"version":1,"name":"legacy","rtmp":{"enable_tls":true,"address":":1936"},
            "api":{"auth":{"enable":true,"username":"admin"}}})
            .to_string(),
        )
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations WHERE success")
            .fetch_one(&pool)
            .await
            .unwrap(),
        4
    );
    pool.close().await;
    let migrated = Instance::open(&dir, &[]).await;
    let (status, tokens) = migrated.login(PASSWORD).await;
    assert_eq!(status, 200);
    let (_, value) = migrated
        .request(
            "GET",
            "/api/v3/config",
            None,
            tokens["access_token"].as_str(),
        )
        .await;
    assert_eq!(value["config"]["version"], 3);
    assert_eq!(value["config"]["name"], "legacy");
    assert_eq!(value["config"]["rtmp"]["address"], ":1935");
}

#[tokio::test]
async fn legacy_localhost_bypass_read_only_network_and_login_throttling() {
    let dir = TempDir::new().unwrap();
    let server = Instance::open(
        &dir,
        &[
            ("CORE_API_AUTH_PASSWORD", PASSWORD),
            ("CORE_API_AUTH_DISABLE_LOCALHOST", "true"),
            ("CORE_API_READ_ONLY", "true"),
            ("CORE_API_ACCESS_HTTP_BLOCK", "192.0.2.0/24"),
        ],
    )
    .await;
    assert_eq!(
        server.request("GET", "/api/v3/config", None, None).await.0,
        200
    );
    assert_eq!(
        server
            .request("PUT", "/api/v3/config", Some(json!({"version":3})), None)
            .await
            .0,
        405
    );
    assert_eq!(
        server
            .request("GET", "/api/v3/config/reload", None, None)
            .await
            .0,
        404
    );
    assert_eq!(
        call(&server.router, "GET", "/api", None, None, "192.0.2.1:1")
            .await
            .0,
        403
    );
    assert_eq!(
        call(
            &server.router,
            "GET",
            "/api/v3/config",
            None,
            None,
            "198.51.100.1:1"
        )
        .await
        .0,
        403
    );
    for _ in 0..8 {
        assert_eq!(
            call(
                &server.router,
                "POST",
                "/api/login",
                Some("{".into()),
                None,
                "127.0.0.1:1"
            )
            .await
            .0,
            403
        );
    }
    assert_eq!(server.login(PASSWORD).await.0, 429);
}

#[tokio::test]
async fn unsafe_listener_and_missing_initial_credentials_fail_startup() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("startup.sqlite3");
    for (key, value) in [("CORE_ADDRESS", ":8080"), ("CORE_API_AUTH_ENABLE", "true")] {
        let environment = Environment::from_lookup(|name| match name {
            "CORE_DATABASE_PATH" => path.to_str().map(str::to_owned),
            _ if name == key => Some(value.to_owned()),
            _ => None,
        })
        .unwrap();
        assert!(App::open(environment).await.is_err());
    }
}

#[tokio::test]
async fn reload_rebinds_listener_and_preserves_it_if_the_new_port_is_unavailable() {
    use axum::serve::Listener;
    use tokio::net::{TcpListener, TcpStream};

    let dir = TempDir::new().unwrap();
    let instance = Instance::open(&dir, &[]).await;
    assert_eq!(
        instance
            .request(
                "PUT",
                "/api/v3/config",
                Some(json!({"version":3,"address":"127.0.0.1:0"})),
                None
            )
            .await
            .0,
        200
    );
    instance.app.close().await;
    drop(instance);

    let path = dir.path().join("foundation.sqlite3");
    let environment = Environment::from_lookup(|name| {
        (name == "CORE_DATABASE_PATH").then(|| path.to_str().unwrap().to_owned())
    })
    .unwrap();
    let mut state = App::open(environment).await.unwrap();
    let mut listener = state.bind().await.unwrap();
    let original = listener.local_addr().unwrap();
    let router = router(Arc::new(state));
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proposed = occupied.local_addr().unwrap();
    assert_ne!(original, proposed);
    let patch = json!({"version":3,"address":proposed.to_string()});
    assert_eq!(
        call(
            &router,
            "PUT",
            "/api/v3/config",
            Some(patch.to_string()),
            None,
            "127.0.0.1:1"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            &router,
            "GET",
            "/api/v3/config/reload",
            None,
            None,
            "127.0.0.1:1"
        )
        .await
        .0,
        409
    );
    assert_eq!(listener.local_addr().unwrap(), original);
    drop(occupied);
    assert_eq!(
        call(
            &router,
            "GET",
            "/api/v3/config/reload",
            None,
            None,
            "127.0.0.1:1"
        )
        .await
        .0,
        200
    );
    let _client = TcpStream::connect(proposed).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept())
        .await
        .unwrap();
    assert_eq!(listener.local_addr().unwrap(), proposed);
    assert!(TcpStream::connect(original).await.is_err());
}
