#![forbid(unsafe_code)]

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use restreamer_core::{App, config::Environment, router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

struct Server {
    state: Arc<App>,
    api: Router,
    token: Option<String>,
}

impl Server {
    async fn open(dir: &TempDir, overrides: &[(&str, &str)]) -> Self {
        let db = dir.path().join("processes.sqlite3");
        let environment = Environment::from_lookup(|name| {
            if name == "CORE_DATABASE_PATH" {
                return db.to_str().map(str::to_owned);
            }
            overrides
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
        .unwrap();
        let state = Arc::new(App::open(environment).await.unwrap());
        let api = router(state.clone());
        Self {
            state,
            api,
            token: None,
        }
    }

    async fn request(&self, method: &str, uri: &str, data: Option<Value>) -> (u16, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .extension(ConnectInfo("127.0.0.1:9000".parse::<SocketAddr>().unwrap()));
        if let Some(token) = &self.token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = self
            .api
            .clone()
            .oneshot(
                request
                    .body(data.map_or_else(Body::empty, |data| Body::from(data.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 20 * 1024 * 1024)
            .await
            .unwrap();
        let payload = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            panic!(
                "HTTP {method} {uri} returned {status}: {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        (status, payload)
    }

    async fn expect(&self, code: u16, method: &str, uri: &str, data: Option<Value>) -> Value {
        let (status, value) = self.request(method, uri, data).await;
        assert_eq!(status, code, "{method} {uri}: {value}");
        value
    }

    async fn order(&self, id: &str, command: &str) {
        assert_eq!(
            self.expect(
                200,
                "PUT",
                &format!("/api/v3/process/{id}/command"),
                Some(json!({"command":command}))
            )
            .await,
            "OK"
        );
    }

    async fn wait_state(&self, id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let limit = Instant::now() + Duration::from_secs(12);
        loop {
            let state = self
                .expect(200, "GET", &format!("/api/v3/process/{id}/state"), None)
                .await;
            if predicate(&state) {
                return state;
            }
            assert!(Instant::now() < limit, "last state for {id}: {state}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_reports(&self, id: &str, count: usize) -> Value {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let report = self
                .expect(200, "GET", &format!("/api/v3/process/{id}/report"), None)
                .await;
            if report["history"]
                .as_array()
                .is_some_and(|history| history.len() >= count)
            {
                return report;
            }
            assert!(
                Instant::now() < deadline,
                "expected {count} reports for {id}: {report}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn synthetic(id: &str) -> Value {
    json!({
        "id":id,"input":[{"id":"source","address":"testsrc2=size=64x64:rate=12","options":["-re","-f","lavfi"]}],
        "output":[{"id":"end","address":"-","options":["-f","null"]}],
        "options":["-loglevel","info"],"autostart":false
    })
}

#[cfg(target_os = "linux")]
fn mock(path: &std::path::Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\nif [ \"$1\" = -version ]; then echo 'ffmpeg version fixture'; exit 0; fi\n{script}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(target_os = "linux")]
fn running(pid: i64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|line| {
            line.rsplit_once(") ")
                .map(|(_, rest)| !rest.starts_with('Z'))
        })
        .unwrap_or(false)
}

#[cfg(windows)]
fn running(pid: i64) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut system = System::new().unwrap();
    let pid = Pid::from_u32(u32::try_from(pid).unwrap());
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some()
}

#[tokio::test]
async fn api_contract_authorization_metadata_reports_and_filters() {
    let dir = TempDir::new().unwrap();
    let password = "process-fixture-admin-42";
    let mut server = Server::open(&dir, &[("CORE_API_AUTH_PASSWORD", password)]).await;
    assert_eq!(
        server.expect(403, "GET", "/api/v3/process", None).await["details"],
        json!(["access denied"])
    );
    let tokens = server
        .expect(
            200,
            "POST",
            "/api/login",
            Some(json!({"username":"admin","password":password})),
        )
        .await;
    server.token = Some(tokens["access_token"].as_str().unwrap().into());
    let base = "/api/v3/process";
    assert_eq!(server.expect(200, "GET", base, None).await, json!([]));
    let mut cfg = synthetic("fixture:01");
    cfg["reference"] = json!("ref");
    let validated = server
        .expect(200, "POST", "/api/v3/process/validate", Some(cfg.clone()))
        .await;
    assert_eq!(validated["zone"], "");
    assert_eq!(validated["limits"]["cpu_usage"], 0.0);
    let created = server.expect(200, "POST", base, Some(cfg.clone())).await;
    assert_eq!(created["zone"], "$default");
    assert_eq!(created["owner"], "admin");
    assert_eq!(created["input"][0]["cleanup"], json!([]));
    server.expect(409, "POST", base, Some(cfg)).await;
    server
        .expect(400, "POST", base, Some(json!({"id":"bad","type":"shell"})))
        .await;
    let details = "/api/v3/process/fixture:01";
    let process = server
        .expect(200, "GET", &format!("{details}?filter=state"), None)
        .await;
    assert!(process.get("config").is_none());
    assert_eq!(process["state"]["exec"], "finished");
    assert_eq!(process["state"]["order"], "stop");
    assert_eq!(process["state"]["pid"], -1);
    assert_eq!(
        process["state"]["progress"]["mapping"],
        json!({"graphs":[],"mapping":[]})
    );
    let filtered = server
        .expect(
            200,
            "GET",
            "/api/v3/process?id=fixture%3A01&reference=wrong&refpattern=r*&filter=config",
            None,
        )
        .await;
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    let updated = server
        .expect(
            200,
            "PUT",
            details,
            Some(json!({"reference":"updated","limits":{"memory_mbytes":100}})),
        )
        .await;
    assert_eq!(updated["reference"], "updated");
    assert_eq!(updated["input"], created["input"]);
    assert_eq!(
        server
            .expect(200, "GET", &format!("{details}/config"), None)
            .await,
        updated
    );
    assert_eq!(
        server
            .expect(200, "GET", &format!("{details}/metadata"), None)
            .await,
        Value::Null
    );
    let metadata = json!({"settings":[1,2]});
    assert_eq!(
        server
            .expect(
                200,
                "PUT",
                &format!("{details}/metadata/ui"),
                Some(metadata.clone())
            )
            .await,
        metadata
    );
    assert_eq!(
        server
            .expect(200, "GET", &format!("{details}/metadata/ui"), None)
            .await,
        metadata
    );
    assert_eq!(
        server
            .expect(200, "GET", &format!("{details}/metadata"), None)
            .await["ui"],
        metadata
    );
    server
        .expect(200, "PUT", "/api/v3/metadata/ui", Some(metadata.clone()))
        .await;
    assert_eq!(
        server.expect(200, "GET", "/api/v3/metadata/ui", None).await,
        metadata
    );
    assert_eq!(
        server.expect(200, "GET", "/api/v3/metadata", None).await["ui"],
        metadata
    );
    server
        .expect(
            200,
            "PUT",
            &format!("{details}/metadata/ui"),
            Some(Value::Null),
        )
        .await;
    assert_eq!(
        server
            .expect(404, "GET", &format!("{details}/metadata/ui"), None)
            .await["details"],
        json!(["unknown metadata key"])
    );
    assert_eq!(
        server
            .expect(200, "GET", &format!("{details}/report"), None)
            .await,
        json!({"created_at":-62135596800i64,"history":[]})
    );
    server.expect(200,"PUT",&format!("{details}/report"),Some(json!({"created_at":0,"history":[
        {"created_at":123,"exited_at":124,"exit_state":"failed","log":[["123","failure from https://user:password@example.test/private?token=some-secret"]]}
    ]}))).await;
    let entry = server
        .expect(
            200,
            "GET",
            &format!("{details}/report?created_at=123"),
            None,
        )
        .await;
    assert_eq!(entry["history"][0]["exit_state"], "failed");
    assert!(!entry.to_string().contains("some-secret"));
    let search = server
        .expect(
            200,
            "GET",
            "/api/v3/report/process?idpattern=fixture*&state=failed&from=120&to=129",
            None,
        )
        .await;
    assert_eq!(search[0]["id"], "fixture:01");
    server
        .expect(
            400,
            "PUT",
            &format!("{details}/command"),
            Some(json!({"command":"bad"})),
        )
        .await;
    server
        .expect(200, "DELETE", &format!("{details}?purge=false"), None)
        .await;
    server.expect(404, "GET", details, None).await;
    server.state.close().await;
}

#[tokio::test]
async fn real_ffmpeg_ffprobe_and_restart_reconciliation() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("input.mkv");
    let output = dir.path().join("output.mkv");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x64:rate=10",
            "-t",
            "2",
            "-c:v",
            "mpeg4",
        ])
        .arg(&input)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let server = Server::open(&dir, &[]).await;
    let skills = server.expect(200, "GET", "/api/v3/skills", None).await;
    assert!(!skills["ffmpeg"]["version"].as_str().unwrap().is_empty());
    assert!(
        !skills["ffmpeg"]["configuration"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert!(
        skills["codecs"]["video"]
            .as_array()
            .unwrap()
            .iter()
            .any(|codec| codec["id"] == "mpeg4")
    );
    assert!(
        skills["protocols"]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|proto| proto["id"] == "file")
    );
    assert!(skills["ffprobe_available"].as_bool().unwrap());
    assert!(!skills["ffprobe_version"].as_str().unwrap().is_empty());
    server
        .expect(200, "GET", "/api/v3/skills/reload", None)
        .await;
    let probe = server
        .expect(
            200,
            "POST",
            "/api/v3/process/probe",
            Some(json!({"input":[{"address":input}]})),
        )
        .await;
    assert_eq!(probe["streams"][0]["codec"], "mpeg4");
    assert_eq!(probe["streams"][0]["width"], 64);
    let cfg = json!({"id":"sample","input":[{"id":"input","address":input,"options":["-stream_loop","-1","-re"]}],
        "output":[{"id":"output","address":output,"options":["-c:v","mpeg4"]}],"options":["-y"],"autostart":true
    });
    server
        .expect(200, "POST", "/api/v3/process", Some(cfg))
        .await;
    server
        .expect(
            200,
            "PUT",
            "/api/v3/process/sample/metadata/key",
            Some(json!({"durable":true})),
        )
        .await;
    let ready = server
        .wait_state("sample", |state| {
            state["exec"] == "running" && state["progress"]["frame"].as_u64().unwrap_or(0) > 0
        })
        .await;
    assert_eq!(ready["progress"]["inputs"][0]["id"], "input");
    let old = ready["pid"].as_i64().unwrap();
    assert!(running(old));
    let ((), (), ()) = tokio::join!(
        server.order("sample", "start"),
        server.order("sample", "start"),
        server.order("sample", "start")
    );
    assert_eq!(
        server
            .expect(200, "GET", "/api/v3/process/sample/state", None)
            .await["pid"],
        old
    );
    server.order("sample", "restart").await;
    let restarted = server
        .wait_state("sample", |state| {
            state["exec"] == "running"
                && state["pid"] != old
                && state["progress"]["frame"].as_u64().unwrap_or(0) > 0
        })
        .await;
    let restarted_pid = restarted["pid"].as_i64().unwrap();
    assert!(!running(old));
    server.wait_reports("sample", 1).await;
    server.state.close().await;
    assert!(!running(restarted_pid));
    assert!(
        tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "csv=p=0"
            ])
            .arg(&output)
            .output()
            .await
            .is_ok_and(|probe| probe.status.success()
                && String::from_utf8_lossy(&probe.stdout).trim() == "mpeg4"),
        "the restarted FFmpeg job did not finalize a readable video"
    );
    drop(server);

    let server = Server::open(&dir, &[]).await;
    let current = server
        .wait_state("sample", |state| {
            state["exec"] == "running" && state["pid"] != restarted_pid
        })
        .await;
    assert_eq!(current["order"], "start");
    assert_eq!(
        server
            .expect(200, "GET", "/api/v3/process/sample/metadata/key", None)
            .await,
        json!({"durable":true})
    );
    assert_eq!(
        server
            .expect(200, "GET", "/api/v3/process/sample/probe", None)
            .await["streams"][0]["codec"],
        "mpeg4"
    );
    server.order("sample", "stop").await;
    let down = server
        .wait_state("sample", |state| {
            state["exec"] == "finished" && state["order"] == "stop"
        })
        .await;
    assert_eq!(down["pid"], -1);
    assert!(
        server.wait_reports("sample", 3).await["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["progress"]["frame"].as_u64().unwrap_or(0) > 0)
    );
    server.state.close().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_jobs_back_off_then_exhaust_and_missing_executables_are_reported() {
    let dir = TempDir::new().unwrap();
    let ffmpeg = dir.path().join("fail-ffmpeg");
    mock(&ffmpeg, "echo 'fixture failure' >&2\nexit 7");
    let server = Server::open(&dir, &[("CORE_FFMPEG_BINARY", ffmpeg.to_str().unwrap())]).await;
    let mut cfg = synthetic("failing");
    cfg["autostart"] = json!(true);
    cfg["reconnect"] = json!(true);
    cfg["reconnect_delay_seconds"] = json!(1);
    cfg["reconnect_max_retries"] = json!(2);
    let started = Instant::now();
    server
        .expect(200, "POST", "/api/v3/process", Some(cfg))
        .await;
    let report = server.wait_reports("failing", 3).await;
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert_eq!(report["history"][0]["exit_code"], 7);
    assert_eq!(report["history"][0]["exit_state"], "failed");
    let state = server
        .wait_state("failing", |state| {
            state["reconnect_seconds"] == -1 && state["restart_count"] == 2
        })
        .await;
    assert_eq!(state["order"], "start");
    server.state.close().await;

    let absent = dir.path().join("missing");
    let server = Server::open(&dir, &[("CORE_FFMPEG_BINARY", absent.to_str().unwrap())]).await;
    server.expect(503, "GET", "/api/v3/skills", None).await;
    server
        .expect(
            503,
            "POST",
            "/api/v3/process/probe",
            Some(synthetic("missing")),
        )
        .await;
    server
        .expect(200, "POST", "/api/v3/process", Some(synthetic("missing")))
        .await;
    server.order("missing", "start").await;
    let report = server.wait_reports("missing", 1).await;
    assert_eq!(
        report["history"][0]["exit_reason"],
        "configured FFmpeg executable could not start"
    );
    server.state.close().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn stall_kills_the_process_group_and_queued_work_resumes() {
    let dir = TempDir::new().unwrap();
    let ffmpeg = dir.path().join("stall-ffmpeg");
    let child_pid = dir.path().join("descendant");
    mock(
        &ffmpeg,
        &format!(
            "trap '' TERM\nsleep 600 &\necho $! > '{}'\nwait",
            child_pid.display()
        ),
    );
    let server = Server::open(
        &dir,
        &[
            ("CORE_FFMPEG_BINARY", ffmpeg.to_str().unwrap()),
            ("CORE_FFMPEG_STOP_TIMEOUT_SECONDS", "1"),
            ("CORE_FFMPEG_MAXPROCESSES", "1"),
            ("CORE_PROCESS_CGROUP_ROOT", dir.path().to_str().unwrap()),
        ],
    )
    .await;
    let mut first = synthetic("first");
    first["autostart"] = json!(true);
    first["stale_timeout_seconds"] = json!(1);
    first["limits"] = json!({"memory_mbytes":1024});
    server
        .expect(200, "POST", "/api/v3/process", Some(first))
        .await;
    let running_state = server
        .wait_state("first", |state| state["exec"] == "running")
        .await;
    let parent = running_state["pid"].as_i64().unwrap();
    let child = loop {
        if let Ok(pid) = std::fs::read_to_string(&child_pid)
            && let Ok(pid) = pid.trim().parse::<i64>()
        {
            break pid;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(running(parent));
    };
    let mut second = synthetic("queued");
    second["autostart"] = json!(true);
    server
        .expect(200, "POST", "/api/v3/process", Some(second))
        .await;
    assert_eq!(
        server
            .expect(200, "GET", "/api/v3/process/queued/state", None)
            .await["pid"],
        -1
    );
    let report = server.wait_reports("first", 1).await;
    assert_eq!(report["history"][0]["exit_state"], "killed");
    assert_eq!(report["history"][0]["exit_reason"], "stale progress");
    assert_eq!(
        report["history"][0]["resources"]["cpu_memory_hard_limit_applied"],
        false
    );
    assert!(!running(parent));
    assert!(!running(child));
    let next = server
        .wait_state("queued", |state| state["exec"] == "running")
        .await["pid"]
        .as_i64()
        .unwrap();
    server.state.close().await;
    assert!(!running(next));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn schedules_survive_restart_and_references_are_validated() {
    let dir = TempDir::new().unwrap();
    let ffmpeg = dir.path().join("scheduled-ffmpeg");
    mock(&ffmpeg, "echo frame=1\necho progress=end\nexit 0");
    let env = [("CORE_FFMPEG_BINARY", ffmpeg.to_str().unwrap())];
    let server = Server::open(&dir, &env).await;
    let mut scheduled = synthetic("scheduled");
    scheduled["scheduler"] = json!(
        jiff::Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(4))
            .unwrap()
            .to_string()
    );
    scheduled["autostart"] = json!(true);
    server
        .expect(200, "POST", "/api/v3/process", Some(scheduled))
        .await;
    let waiting = server
        .wait_state("scheduled", |state| {
            state["reconnect_seconds"].as_i64().unwrap_or(-1) > 0
        })
        .await;
    assert_eq!(waiting["pid"], -1);
    server.state.close().await;
    drop(server);
    let server = Server::open(&dir, &env).await;
    let report = server.wait_reports("scheduled", 1).await;
    assert_eq!(report["history"][0]["exit_state"], "finished");

    let mut a = synthetic("source");
    a["output"][0]["address"] = json!("{diskfs}/{processid}.mkv");
    a["output"][0]["options"] = json!(["-f", "matroska"]);
    server.expect(200, "POST", "/api/v3/process", Some(a)).await;
    let mut b = synthetic("sink");
    b["input"][0]["address"] = json!("#source:output=end");
    b["input"][0]["options"] = json!([]);
    server.expect(200, "POST", "/api/v3/process", Some(b)).await;
    let state = server
        .expect(200, "GET", "/api/v3/process/sink/state", None)
        .await;
    assert!(
        state["command"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "./data/source.mkv")
    );
    server
        .expect(
            400,
            "PUT",
            "/api/v3/process/source",
            Some(json!({"input":[{"address":"#sink:output=end"}]})),
        )
        .await;
    let mut invalid = synthetic("dangling");
    invalid["input"][0]["address"] = json!("#missing:output=end");
    server
        .expect(400, "POST", "/api/v3/process", Some(invalid))
        .await;
    server.state.close().await;
}

#[tokio::test]
async fn input_output_acl_and_read_only_policy() {
    let dir = TempDir::new().unwrap();
    let server = Server::open(
        &dir,
        &[
            ("CORE_FFMPEG_ACCESS_INPUT_BLOCK", "^testsrc"),
            ("CORE_FFMPEG_ACCESS_OUTPUT_BLOCK", "^/forbidden/"),
        ],
    )
    .await;
    server
        .expect(403, "POST", "/api/v3/process", Some(synthetic("denied")))
        .await;
    let mut output = synthetic("output");
    output["input"][0]["address"] = json!("fixture.mkv");
    output["input"][0]["options"] = json!([]);
    output["output"][0]["address"] = json!("/forbidden/channel.mkv");
    server
        .expect(403, "POST", "/api/v3/process", Some(output))
        .await;
    let mut reserved = synthetic("extra-input");
    reserved["options"] = json!(["-i", "/etc/hosts"]);
    server
        .expect(400, "POST", "/api/v3/process", Some(reserved))
        .await;
    server.state.close().await;

    let server = Server::open(&dir, &[("CORE_API_READ_ONLY", "true")]).await;
    server.expect(200, "GET", "/api/v3/process", None).await;
    server
        .expect(404, "POST", "/api/v3/process", Some(synthetic("readonly")))
        .await;
    server
        .expect(404, "PUT", "/api/v3/metadata/key", Some(json!(true)))
        .await;
    server.state.close().await;
}

#[tokio::test]
#[ignore = "repeatable real FFmpeg throughput measurement"]
async fn four_concurrent_ffmpeg_jobs() {
    let dir = TempDir::new().unwrap();
    let server = Server::open(&dir, &[]).await;
    let started = Instant::now();
    let ids = ["perf0", "perf1", "perf2", "perf3"];
    for id in ids {
        let mut config = synthetic(id);
        config["autostart"] = json!(true);
        config["output"][0]["options"] = json!(["-threads", "1", "-t", "2", "-f", "null"]);
        server
            .expect(200, "POST", "/api/v3/process", Some(config))
            .await;
    }
    let active = server
        .expect(200, "GET", "/api/v3/process?filter=state", None)
        .await;
    assert_eq!(active.as_array().unwrap().len(), 4);
    let mut frames = 0;
    for id in ids {
        let report = server.wait_reports(id, 1).await;
        let run = &report["history"][0];
        assert_eq!(run["exit_state"], "finished");
        frames += run["progress"]["frame"].as_u64().unwrap();
    }
    println!(
        "4 FFmpeg jobs, {frames} output frames, wall {:.3}s",
        started.elapsed().as_secs_f64()
    );
    assert!(frames >= 80);
    server.state.close().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dynamic_references_reuse_the_producer_endpoint_and_cleanup_is_scoped() {
    let dir = TempDir::new().unwrap();
    let media_dir = dir.path().join("media");
    let ffmpeg = dir.path().join("dynamic-ffmpeg");
    mock(&ffmpeg, "echo frame=1\necho progress=continue\nsleep 600");
    let server = Server::open(
        &dir,
        &[
            ("CORE_FFMPEG_BINARY", ffmpeg.to_str().unwrap()),
            ("CORE_STORAGE_DISK_DIR", media_dir.to_str().unwrap()),
        ],
    )
    .await;
    let mut producer = synthetic("producer");
    producer["output"][0]["address"] = json!("{diskfs}/capture-{date}.mkv");
    producer["output"][0]["options"] = json!(["-f", "matroska"]);
    producer["output"][0]["cleanup"] =
        json!([{"pattern":"{diskfs}/capture-*.mkv","purge_on_delete":true}]);
    producer["autostart"] = json!(true);
    server
        .expect(200, "POST", "/api/v3/process", Some(producer))
        .await;
    let source = server
        .wait_state("producer", |state| state["exec"] == "running")
        .await;
    let address = source["progress"]["outputs"][0]["address"]
        .as_str()
        .unwrap();
    assert!(address.contains("capture-"));
    // Wait until expanding the same date placeholder independently would produce a different file.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let mut downstream = synthetic("consumer");
    downstream["input"][0] = json!({"id":"linked","address":"#producer:output=end"});
    downstream["autostart"] = json!(true);
    server
        .expect(200, "POST", "/api/v3/process", Some(downstream))
        .await;
    let consumer = server
        .wait_state("consumer", |state| state["exec"] == "running")
        .await;
    assert_eq!(consumer["progress"]["inputs"][0]["address"], address);
    tokio::fs::write(address, b"fixture").await.unwrap();
    let untouched = media_dir.join("unrelated.mkv");
    tokio::fs::write(&untouched, b"fixture").await.unwrap();
    server
        .expect(200, "DELETE", "/api/v3/process/producer", None)
        .await;
    assert!(!std::path::Path::new(address).exists());
    assert!(untouched.exists());
    server.state.close().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn standalone_core_crash_terminates_the_media_leader() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let ffmpeg = dir.path().join("parent-death-ffmpeg");
    let marker = dir.path().join("child-pid");
    let probe_marker = dir.path().join("health-child-pid");
    std::fs::write(&ffmpeg, format!(
        "#!/bin/sh\nif [ \"$1\" = -version ]; then echo $$ > '{}'; else echo $$ > '{}'; fi\nexec sleep 600\n",
        probe_marker.display(), marker.display(),
    )).unwrap();
    std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o700)).unwrap();
    let setup = Server::open(&dir, &[]).await;
    setup
        .expect(200, "POST", "/api/v3/process", Some(synthetic("crash")))
        .await;
    setup.state.close().await;
    drop(setup);

    let db = dir.path().join("processes.sqlite3");
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&db))
            .await
            .unwrap();
    sqlx::query("UPDATE process_jobs SET desired=1 WHERE id='crash'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let mut core = tokio::process::Command::new(env!("CARGO_BIN_EXE_restreamer-core"))
        .env("CORE_DATABASE_PATH", &db)
        .env("CORE_ADDRESS", "127.0.0.1:0")
        .env("CORE_FFMPEG_BINARY", &ffmpeg)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let (pid, probe) = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let (Ok(file), Ok(health)) = (
                tokio::fs::read_to_string(&marker).await,
                tokio::fs::read_to_string(&probe_marker).await,
            ) && let (Ok(pid), Ok(probe)) =
                (file.trim().parse::<i64>(), health.trim().parse::<i64>())
            {
                break (pid, probe);
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert!(running(pid));
    core.kill().await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while running(pid) || running(probe) {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn ffprobe_timeout_reaps_its_child_and_returns_an_api_error() {
    let dir = TempDir::new().unwrap();
    let ffprobe = dir.path().join("slow-ffprobe");
    let marker = dir.path().join("probe-pid");
    mock(
        &ffprobe,
        &format!("echo $$ > '{}'\nexec sleep 600", marker.display()),
    );
    let server = Server::open(
        &dir,
        &[
            ("CORE_FFPROBE_BINARY", ffprobe.to_str().unwrap()),
            ("CORE_FFMPEG_PROBE_TIMEOUT_SECONDS", "1"),
        ],
    )
    .await;
    let report = server
        .expect(
            503,
            "POST",
            "/api/v3/process/probe",
            Some(synthetic("probe-timeout")),
        )
        .await;
    assert_eq!(report["details"], json!(["media command timed out"]));
    let pid = tokio::fs::read_to_string(marker)
        .await
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!running(pid));
    server.state.close().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a disposable delegated cgroup v2 with cpu and memory controllers"]
async fn kernel_limits_attach_and_remove_an_ffmpeg_cgroup() {
    let root = std::path::PathBuf::from(std::env::var_os("CASTVANE_TEST_CGROUP_ROOT").unwrap());
    let dir = TempDir::new().unwrap();
    let ffmpeg = dir.path().join("ffmpeg");
    mock(&ffmpeg, "echo frame=1\necho progress=continue\nsleep 600");
    let server = Server::open(
        &dir,
        &[
            ("CORE_PROCESS_CGROUP_ROOT", root.to_str().unwrap()),
            ("CORE_FFMPEG_BINARY", ffmpeg.to_str().unwrap()),
        ],
    )
    .await;
    let mut config = synthetic("hard-limits");
    config["limits"] = json!({"cpu_usage":25.0,"memory_mbytes":32});
    config["autostart"] = json!(true);
    server
        .expect(200, "POST", "/api/v3/process", Some(config))
        .await;
    let path = "/api/v3/process/hard-limits/state";
    let state = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let state = server.expect(200, "GET", path, None).await;
            if state["limit_mode"] == "hard" {
                break state;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("FFmpeg did not enter a delegated cgroup");
    let pid = state["pid"].as_u64().unwrap();
    let mut entries = tokio::fs::read_dir(&root).await.unwrap();
    let path = loop {
        let entry = entries
            .next_entry()
            .await
            .unwrap()
            .expect("child cgroup missing");
        let path = entry.path();
        if entry.file_type().await.unwrap().is_dir()
            && tokio::fs::read_to_string(path.join("cgroup.procs"))
                .await
                .unwrap_or_default()
                .lines()
                .any(|value| value.parse() == Ok(pid))
        {
            break path;
        }
    };
    assert_eq!(
        std::fs::read_to_string(path.join("cpu.max"))
            .unwrap()
            .trim(),
        "25000 100000"
    );
    assert_eq!(
        std::fs::read_to_string(path.join("memory.max"))
            .unwrap()
            .trim(),
        "33554432"
    );
    server.state.close().await;
    assert!(!path.exists(), "child cgroup survived shutdown");
}
