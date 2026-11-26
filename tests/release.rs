#![forbid(unsafe_code)]

use std::{net::SocketAddr, os::unix::fs::PermissionsExt, path::Path, sync::Arc};

use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use restreamer_core::{App, config::Environment, maintenance, migration, router};
use serde_json::{Value, json};
use tower::ServiceExt;

fn environment(database: &Path, media: &Path) -> Environment {
    Environment::from_lookup(|key| match key {
        "CORE_DATABASE_PATH" => database.to_str().map(str::to_owned),
        "CORE_STORAGE_DISK_DIR" => media.to_str().map(str::to_owned),
        "CORE_API_AUTH_PASSWORD" => Some("fixture-only-password".into()),
        "CORE_METRICS_ENABLE" => Some("true".into()),
        _ => None,
    })
    .unwrap()
}

async fn request(
    app: &Arc<App>,
    method: &str,
    path: &str,
    token: Option<&str>,
    value: Value,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .extension(ConnectInfo(
            "127.0.0.1:12017".parse::<SocketAddr>().unwrap(),
        ));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router(app.clone())
        .oneshot(
            request
                .header("content-type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    (
        response.status(),
        to_bytes(response.into_body(), 5 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
}

async fn login(app: &Arc<App>) -> String {
    let (status, body) = request(
        app,
        "POST",
        "/api/login",
        None,
        json!({
            "username":"admin", "password":"fixture-only-password"
        }),
    )
    .await;
    assert_eq!(status, 200);
    serde_json::from_slice::<Value>(&body).unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn graph(app: &Arc<App>, token: Option<&str>, query: &str) -> (StatusCode, Value) {
    let (status, body) = request(
        app,
        "POST",
        "/api/graph/query",
        token,
        json!({"query":query}),
    )
    .await;
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn graphql_legacy_queries_and_limits() {
    let temp = tempfile::tempdir().unwrap();
    let app = Arc::new(
        App::open(environment(
            &temp.path().join("db"),
            &temp.path().join("media"),
        ))
        .await
        .unwrap(),
    );
    assert_eq!(graph(&app, None, "{ ping }").await.0, 403);
    let token = login(&app).await;
    let (status, body) = graph(
        &app,
        Some(&token),
        "{ ping about { id version { number } } processes { id } log }",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["data"]["ping"], "pong", "{body}");
    assert_eq!(
        body["data"]["about"]["version"]["number"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(body["data"]["processes"], json!([]));
    assert_eq!(
        graph(&app, Some(&token), "mutation { ping }").await.1["data"]["ping"],
        "pong"
    );
    let (status, _) = request(
        &app,
        "POST",
        "/api/v3/process",
        Some(&token),
        json!({
            "id":"phase6-fixture","input":[{"id":"in","address":"/example/in.mkv"}],
            "output":[{"id":"out","address":"-","options":["-f","null"]}]
        }),
    )
    .await;
    assert_eq!(status, 200);
    let (_, body) = graph(&app, Some(&token),
        r#"{ processes(idpattern:"phase6-*") { id state { state progress { input { id } } } config { input { id } limits { memory_bytes } } report { history { created_at } prelude log { data } } } }"#,
    ).await;
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(
        body["data"]["processes"][0]["config"]["input"][0]["id"],
        "in"
    );
    assert_eq!(request(&app, "POST", "/api/v3/iam/user", Some(&token), json!({
        "name":"reader","auth":{"api":{"userpass":"fixture-reader-password"}},
        "policies":[{"domain":"$none","types":["process"],"resource":"phase6-*","actions":["read"]}]
    })).await.0, 200);
    let (status, tokens) = request(
        &app,
        "POST",
        "/api/login",
        None,
        json!({
            "username":"reader", "password":"fixture-reader-password"
        }),
    )
    .await;
    assert_eq!(status, 200);
    let tokens: Value = serde_json::from_slice(&tokens).unwrap();
    let reader = tokens["access_token"].as_str().unwrap();
    assert_eq!(
        graph(&app, Some(reader), "{ processes { id } }").await.1["data"]["processes"][0]["id"],
        "phase6-fixture"
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v3/process/phase6-fixture/probe",
            Some(reader),
            Value::Null
        )
        .await
        .0,
        403
    );
    assert_eq!(
        graph(
            &app,
            Some(reader),
            r#"{ probe(id:"phase6-fixture",domain:""){log} }"#
        )
        .await
        .1["errors"][0]["message"],
        "forbidden"
    );

    let (_, body) = graph(&app, Some(&token), "{ thisDoesNotExist }").await;
    assert!(body["errors"].is_array());
    let heavy = (0..240).map(|i| format!("a{i}:ping ")).collect::<String>();
    let (_, body) = graph(&app, Some(&token), &format!("{{{heavy}}}")).await;
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("complex"),
        "{body}"
    );
    assert_eq!(
        graph(
            &app,
            Some(&token),
            &format!("{{ ping }}#{}", "x".repeat(130_000))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        graph(
            &app,
            Some(&token),
            &format!("{{ ping }}#{}", "x".repeat(140_000))
        )
        .await
        .0,
        413
    );
    let (status, schema) = request(
        &app,
        "GET",
        "/api/graph/schema.graphql",
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert!(String::from_utf8(schema).unwrap().contains("type Process"));
    app.close().await;
}

#[tokio::test]
async fn consistent_live_backup_with_integrity_and_no_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let source = root.join("source");
    let output = root.join("backup");
    let restored = root.join("restored");
    let media = root.join("media");
    let app = Arc::new(App::open(environment(&source, &media)).await.unwrap());
    let token = login(&app).await;
    assert_eq!(
        request(
            &app,
            "PUT",
            "/api/v3/metadata/test",
            Some(&token),
            json!({"v":6})
        )
        .await
        .0,
        200
    );
    let checksum = maintenance::backup(&source, &output).await.unwrap();
    assert_eq!(
        std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(maintenance::backup(&source, &output).await.is_err());
    assert!(
        maintenance::restore(output.clone(), restored.clone(), "0".repeat(64))
            .await
            .is_err()
    );
    assert!(!restored.exists());
    maintenance::restore(output.clone(), restored.clone(), checksum.clone())
        .await
        .unwrap();
    assert!(
        maintenance::restore(output, restored.clone(), checksum)
            .await
            .is_err()
    );
    maintenance::database_check(&restored).await.unwrap();
    let app2 = Arc::new(App::open(environment(&restored, &media)).await.unwrap());
    let (status, value) = request(
        &app2,
        "GET",
        "/api/v3/metadata/test",
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&value).unwrap(),
        json!({"v":6})
    );
    app2.close().await;
    app.close().await;
}

#[tokio::test]
async fn migration_rehearsal_and_transactional_publication() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config = root.join("config.json");
    let processes = root.join("db.json");
    let zones = root.join("zones.json");
    let aliases = root.join("aliases.json");
    let users = root.join("users.json");
    let policy = root.join("policy.json");
    let target = root.join("castvane.sqlite3");
    let media = root.join("media");
    let env = environment(&target, &media);
    let document = json!({
        "version":4,"process":{"example":{
            "id":"example","domain":"","order":"start","created_at":100,"updated_at":200,
            "config":{"id":"example","autostart":true,"zone":"backup","timeout":10,"scheduler":"0 0 * * *",
                "limit_memory_bytes":1048577,
                "input":[{"id":"in","address":"clip.mkv","options":[]}],
                "output":[{"id":"out","address":"-","options":["-f","null"]}]}
        }},
        "metadata":{"process":{"example":{"testing":{"version":6}}},"system":{"core":["fixture"]}}
    }).to_string();
    std::fs::write(&config, r#"{"version":3,"name":"imported"}"#).unwrap();
    std::fs::write(&processes, &document).unwrap();
    std::fs::write(
        &zones,
        r#"{"version":1,"zones":[{"id":"backup","limits":{"memory_bytes":8388608}}]}"#,
    )
    .unwrap();
    std::fs::write(
        &aliases,
        r#"{"version":1,"rules":[{"pattern":"/example","target":"/index.m3u8"}]}"#,
    )
    .unwrap();
    std::fs::write(&users, r#"[{"name":"legacy","alias":"Legacy","superuser":false,"created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-02T00:00:00Z","auth":{"api":{"password":"untrusted-old-secret"}}}]"#).unwrap();
    std::fs::write(&policy, r#"[{"name":"$none","policies":[{"username":"legacy","resource":"process:exam*","actions":"read"}]}]"#).unwrap();
    let run = |output| {
        migration::migrate(
            migration::Source {
                config: &config,
                processes: &processes,
                zones: Some(&zones),
                aliases: Some(&aliases),
                users: Some(&users),
                policies: Some(&policy),
            },
            output,
            &env,
        )
    };
    std::fs::write(
        &processes,
        r#"{"version":4,"process":{"example":{},"example":{}}}"#,
    )
    .unwrap();
    let duplicate = run(None).await.unwrap_err().to_string();
    assert!(
        duplicate.contains("duplicate JSON object key"),
        "{duplicate}"
    );
    assert!(!target.exists());
    std::fs::write(&processes, &document).unwrap();
    let report = run(None).await.unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["processes_previously_enabled"], 1);
    assert_eq!(report["users"], 1);
    assert_eq!(report["policies"], 1);
    assert!(!target.exists());
    assert_eq!(run(Some(&target)).await.unwrap()["processes"], 1);
    assert!(run(Some(&target)).await.is_err());
    assert_eq!(std::fs::read_to_string(&processes).unwrap(), document);
    let app = Arc::new(App::open(env).await.unwrap());
    let token = login(&app).await;
    let (status, value) = request(
        &app,
        "GET",
        "/api/v3/process/example",
        Some(&token),
        Value::Null,
    )
    .await;
    let result: Value = serde_json::from_slice(&value).unwrap();
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["config"]["autostart"], false);
    assert_eq!(result["config"]["runtime_duration_seconds"], 10);
    assert_eq!(result["config"]["limits"]["memory_mbytes"], 2);
    assert_eq!(result["state"]["order"], "stop");
    assert_eq!(result["metadata"]["testing"]["version"], 6);
    let (status, account) = request(
        &app,
        "GET",
        "/api/v3/iam/user/legacy",
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    let account: Value = serde_json::from_slice(&account).unwrap();
    assert_eq!(account["active"], false);
    assert_eq!(account["policies"][0]["resource"], "exam*");
    assert!(
        !std::fs::read(&target)
            .unwrap()
            .windows(b"untrusted-old-secret".len())
            .any(|window| window == b"untrusted-old-secret")
    );
    app.close().await;
}
