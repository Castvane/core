#![forbid(unsafe_code)]

use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::ConnectInfo,
    http::{HeaderMap, Request},
    serve::ListenerExt,
};
use restreamer_core::{App, config::Environment, router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

struct Fixture {
    app: Arc<App>,
    router: Router,
    origin: String,
    stop: tokio_util::sync::CancellationToken,
}

impl Fixture {
    async fn open(dir: &TempDir, env: &[(&str, &str)]) -> Self {
        let disk = dir.path().join("files");
        let database = dir.path().join("media.sqlite3");
        let settings = Environment::from_lookup(|name| match name {
            "CORE_DATABASE_PATH" => database.to_str().map(str::to_owned),
            "CORE_STORAGE_DISK_DIR" => disk.to_str().map(str::to_owned),
            "CORE_BIND_ADDR" => Some("127.0.0.1:0".into()),
            _ => env
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).into()),
        })
        .unwrap();
        let mut app = App::open(settings).await.unwrap();
        let listener = app.bind().await.unwrap();
        let addr = axum::serve::Listener::local_addr(&listener).unwrap();
        let app = Arc::new(app);
        let router = router(app.clone());
        let server = router.clone();
        let stop = tokio_util::sync::CancellationToken::new();
        tokio::spawn(restreamer_core::serve_http(
            listener.tap_io(|_| {}),
            server,
            stop.clone(),
        ));
        Self {
            app,
            router,
            origin: format!("http://{addr}"),
            stop,
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> axum::response::Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .extension(ConnectInfo("127.0.0.1:2000".parse::<SocketAddr>().unwrap()));
        for &(name, val) in headers {
            request = request.header(name, val);
        }
        self.router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn expect(&self, code: u16, method: &str, path: &str, body: Body) -> (HeaderMap, Bytes) {
        let response = self.request(method, path, &[], body).await;
        let (parts, body) = response.into_parts();
        let body = to_bytes(body, 32 * 1024 * 1024).await.unwrap();
        assert_eq!(
            parts.status.as_u16(),
            code,
            "{method} {path}: {}",
            String::from_utf8_lossy(&body)
        );
        (parts.headers, body)
    }

    async fn json(&self, code: u16, method: &str, path: &str, body: Option<Value>) -> Value {
        let response = self
            .request(
                method,
                path,
                &[("content-type", "application/json")],
                body.map_or_else(Body::empty, |body| Body::from(body.to_string())),
            )
            .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(
            status.as_u16(),
            code,
            "{method} {path}: {}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn close(self) {
        self.app.stop_media_delivery();
        self.stop.cancel();
        self.app.close().await;
    }
}

#[tokio::test]
async fn disk_http_range_head_alias_and_interrupted_atomic_publication() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(&dir, &[("CORE_STORAGE_DISK_MAX_SIZE_MBYTES", "1")]).await;
    let put = "/api/v3/fs/disk/live/part.ts";
    let get = "/live/part.ts";
    fixture
        .expect(201, "PUT", put, Body::from("0123456789"))
        .await;
    let (headers, _) = fixture.expect(200, "GET", get, Body::empty()).await;
    assert_eq!(headers["content-type"], "video/MP2T");
    assert!(
        headers["cache-control"]
            .to_str()
            .unwrap()
            .starts_with("public")
    );
    let etag = headers["etag"].to_str().unwrap();
    let (headers, empty) = fixture.expect(200, "HEAD", get, Body::empty()).await;
    assert_eq!(headers["content-length"], "10");
    assert!(empty.is_empty());
    let response = fixture
        .request("GET", get, &[("range", "bytes=2-5")], Body::empty())
        .await;
    assert_eq!(response.status(), 206);
    assert_eq!(response.headers()["content-range"], "bytes 2-5/10");
    assert_eq!(to_bytes(response.into_body(), 32).await.unwrap(), "2345");
    let multi = fixture
        .request("GET", get, &[("range", "bytes=0-1,8-9")], Body::empty())
        .await;
    assert_eq!(multi.status(), 206);
    assert!(
        multi.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("multipart/byteranges")
    );
    let expected_length = multi.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let data = to_bytes(multi.into_body(), 1024).await.unwrap();
    assert_eq!(data.len(), expected_length);
    let data = std::str::from_utf8(&data).unwrap();
    assert!(data.contains("Content-Range: bytes 0-1/10\r\n\r\n01"));
    assert!(data.contains("Content-Range: bytes 8-9/10\r\n\r\n89"));
    let invalid = fixture
        .request("GET", get, &[("range", "bytes=99-")], Body::empty())
        .await;
    assert_eq!(invalid.status(), 416);
    assert_eq!(invalid.headers()["content-range"], "bytes */10");
    assert_eq!(
        fixture
            .request("GET", get, &[("if-none-match", etag)], Body::empty())
            .await
            .status(),
        304
    );
    fixture
        .json(
            200,
            "POST",
            "/api/v3/alias",
            Some(json!({"pattern":"^/alias/(.+)$","target":"/live/$1"})),
        )
        .await;
    assert_eq!(
        fixture
            .expect(200, "GET", "/alias/part.ts?filename=clip.ts", Body::empty())
            .await
            .1,
        "0123456789"
    );
    fixture
        .json(
            200,
            "GET",
            "/api/v3/alias/rule?pattern=%5E%2Falias%2F%28.%2B%29%24",
            None,
        )
        .await;
    let cut = Body::from_stream(futures_util::stream::iter([
        Ok(Bytes::from_static(b"accident")),
        Err(std::io::Error::other("fixture disconnect")),
    ]));
    fixture.expect(408, "PUT", put, cut).await;
    assert_eq!(
        fixture.expect(200, "GET", get, Body::empty()).await.1,
        "0123456789"
    );
    fixture
        .expect(507, "PUT", put, Body::from(vec![0; 1024 * 1024 + 1]))
        .await;
    assert_eq!(
        fixture
            .json(200, "GET", "/api/v3/fs/disk?glob=%2Flive%2F*", None)
            .await[0]["name"],
        "/live/part.ts"
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("files/cannot-read.txt"))
            .unwrap();
        let no = fixture
            .request("GET", "/cannot-read.txt", &[], Body::empty())
            .await;
        assert!(!no.status().is_success());
    }
    assert_eq!(
        fixture
            .request("GET", "/live/%2e%2e/part.ts", &[], Body::empty())
            .await
            .status(),
        400
    );
    fixture.close().await;
    let fixture = Fixture::open(&dir, &[]).await;
    assert_eq!(
        fixture
            .expect(200, "GET", "/alias/part.ts", Body::empty())
            .await
            .1,
        "0123456789"
    );
    fixture.expect(200, "DELETE", put, Body::empty()).await;
    fixture.close().await;
}

#[tokio::test]
async fn memory_accounts_for_open_readers_and_copy_move() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(&dir, &[("CORE_STORAGE_MEMORY_MAX_SIZE_MBYTES", "1")]).await;
    let data = vec![b'a'; 750 * 1024];
    fixture
        .expect(201, "PUT", "/api/v3/fs/mem/a.ts", Body::from(data.clone()))
        .await;
    let reader = fixture
        .request("GET", "/memfs/a.ts", &[], Body::empty())
        .await;
    assert_eq!(reader.status(), 200);
    fixture
        .expect(200, "DELETE", "/api/v3/fs/mem/a.ts", Body::empty())
        .await;
    fixture
        .expect(507, "PUT", "/api/v3/fs/mem/b.ts", Body::from(data.clone()))
        .await;
    assert_eq!(
        to_bytes(reader.into_body(), 1024 * 1024).await.unwrap(),
        data
    );
    fixture
        .expect(201, "PUT", "/api/v3/fs/mem/a.ts", Body::from(data))
        .await;
    fixture
        .json(
            200,
            "PUT",
            "/api/v3/fs",
            Some(json!({"operation":"move","source":"mem:/a.ts","target":"disk:/a.ts"})),
        )
        .await;
    fixture
        .expect(404, "GET", "/memfs/a.ts", Body::empty())
        .await;
    assert_eq!(
        fixture
            .expect(200, "GET", "/a.ts", Body::empty())
            .await
            .1
            .len(),
        750 * 1024
    );
    let usage = fixture
        .json(200, "GET", "/api/v3/fs?usage=true", None)
        .await;
    assert_eq!(
        usage
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "mem")
            .unwrap()["usage"]["resident_bytes"],
        0
    );
    fixture.close().await;
}

#[tokio::test]
async fn private_playlists_scope_tokens_and_count_one_viewer() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(
        &dir,
        &[
            ("CORE_STORAGE_PRIVATE", "true"),
            ("CORE_API_AUTH_PASSWORD", "media-fixture-password"),
            ("CORE_SESSIONS_MAX_SESSIONS", "1"),
            ("CORE_SESSIONS_SESSION_TIMEOUT_SEC", "2"),
        ],
    )
    .await;
    let login = fixture
        .json(
            200,
            "POST",
            "/api/login",
            Some(json!({"username":"admin","password":"media-fixture-password"})),
        )
        .await;
    let authorization = format!("Bearer {}", login["access_token"].as_str().unwrap());
    let headers = [("authorization", authorization.as_str())];
    for (path, content) in [
        (
            "main.m3u8",
            "#EXTM3U\n#EXT-X-MAP:URI=\"intro.mp4?existing=1\"\nchunk.ts?existing=2\nhttps://elsewhere.test/other.ts\n",
        ),
        ("chunk.ts", "chunk-data"),
        ("intro.mp4", "init"),
    ] {
        assert_eq!(
            fixture
                .request(
                    "PUT",
                    &format!("/api/v3/fs/disk/level/{path}"),
                    &headers,
                    Body::from(content)
                )
                .await
                .status(),
            201
        );
    }
    fixture
        .expect(403, "GET", "/level/main.m3u8", Body::empty())
        .await;
    assert_eq!(
        fixture
            .request("GET", "/api/v3/fs", &[], Body::empty())
            .await
            .status(),
        403
    );
    let response = fixture
        .request(
            "PUT",
            "/api/v3/session/token/admin",
            &[headers[0], ("content-type", "application/json")],
            Body::from(r#"[{"match":"/level/*","ttl_sec":30}]"#),
        )
        .await;
    assert_eq!(response.status(), 200);
    let data: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    let token = data[0]["token"].as_str().unwrap();
    let uri = format!("/level/main.m3u8?token={token}");
    let (_, body) = fixture.expect(200, "GET", &uri, Body::empty()).await;
    let body = std::str::from_utf8(&body).unwrap();
    assert!(body.contains("https://elsewhere.test/other.ts\n"));
    assert!(body.contains("URI=\"intro.mp4?existing=1&session="));
    let chunk = body
        .lines()
        .find(|line| line.starts_with("chunk.ts"))
        .unwrap();
    assert_eq!(
        fixture
            .expect(200, "GET", &format!("/level/{chunk}"), Body::empty())
            .await
            .1,
        "chunk-data"
    );
    fixture.expect(507, "GET", &uri, Body::empty()).await;
    let view = fixture
        .request(
            "GET",
            "/api/v3/session/active?collectors=hls",
            &headers,
            Body::empty(),
        )
        .await;
    let active: Value =
        serde_json::from_slice(&to_bytes(view.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(active["hls"].as_array().unwrap().len(), 1);
    assert!(active["hls"][0]["bytes_tx"].as_u64().unwrap() > 10);
    tokio::time::sleep(Duration::from_millis(2200)).await;
    fixture.expect(200, "GET", &uri, Body::empty()).await;
    fixture.close().await;
}

async fn generate(dir: &Path, format: &str, name: &str, args: &[&str]) {
    let status = tokio::process::Command::new("ffmpeg")
        .current_dir(dir)
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x64:rate=10",
            "-t",
            "3",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-g",
            "10",
            "-sc_threshold",
            "0",
            "-f",
            format,
        ])
        .args(args)
        .arg(name)
        .status()
        .await
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn ffmpeg_plays_generated_hls_and_dash_via_core_http() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(&dir, &[]).await;
    let disk = dir.path().join("files");
    generate(
        &disk,
        "hls",
        "vod.m3u8",
        &[
            "-hls_time",
            "1",
            "-hls_playlist_type",
            "vod",
            "-hls_flags",
            "temp_file",
        ],
    )
    .await;
    generate(
        &disk,
        "hls",
        "fmp4.m3u8",
        &[
            "-hls_time",
            "1",
            "-hls_segment_type",
            "fmp4",
            "-hls_fmp4_init_filename",
            "fmp4-init.mp4",
            "-hls_flags",
            "temp_file",
        ],
    )
    .await;
    generate(&disk, "dash", "vod.mpd", &["-seg_duration", "1"]).await;
    assert_eq!(
        fixture
            .expect(200, "HEAD", "/vod.mpd", Body::empty())
            .await
            .0["content-type"],
        "application/dash+xml"
    );
    for media in ["vod.m3u8", "fmp4.m3u8", "vod.mpd"] {
        let status = tokio::time::timeout(
            Duration::from_secs(12),
            tokio::process::Command::new("ffmpeg")
                .kill_on_drop(true)
                .args([
                    "-nostdin",
                    "-v",
                    "error",
                    "-i",
                    &format!("{}/{media}", fixture.origin),
                    "-frames:v",
                    "10",
                    "-f",
                    "null",
                    "-",
                ])
                .status(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(status.success(), "{media}");
    }
    fixture.close().await;
}

#[tokio::test]
#[ignore = "requires isolated S3 service provisioned by scripts/test-media-services.sh"]
async fn s3_multipart_ranged_and_public_delivery() {
    let dir = TempDir::new().unwrap();
    let endpoint = std::env::var("PHASE3_S3_ENDPOINT").unwrap();
    let config = json!([
        {"name":"object","mountpoint":"/objects","endpoint":endpoint,
         "region":"us-east-1","bucket":"phase3","access_key_id":"fixture-key","secret_access_key":"fixture-secret","use_ssl":false},
        {"name":"private","mountpoint":"/private-s3","endpoint":endpoint,
         "region":"us-east-1","bucket":"phase3","access_key_id":"fixture-key","secret_access_key":"fixture-secret","use_ssl":false,
         "auth":{"enable":true,"username":"object-user","password":"object-password"}}
    ]).to_string();
    let fixture = Fixture::open(&dir, &[("CORE_STORAGE_S3_JSON", &config)]).await;
    let path = "/api/v3/fs/object/a.mp4";
    let size = 9 * 1024 * 1024;
    fixture
        .expect(201, "PUT", path, Body::from(vec![0x45; size]))
        .await;
    assert_eq!(
        fixture
            .expect(200, "HEAD", "/objects/a.mp4", Body::empty())
            .await
            .0["content-length"],
        size.to_string()
    );
    let response = fixture
        .request(
            "GET",
            "/objects/a.mp4",
            &[("range", "bytes=-64")],
            Body::empty(),
        )
        .await;
    assert_eq!(response.status(), 206);
    assert_eq!(
        to_bytes(response.into_body(), 64).await.unwrap(),
        [0x45; 64].as_slice()
    );
    assert_eq!(
        fixture
            .json(200, "GET", "/api/v3/fs/object?glob=%2Fa.*", None)
            .await[0]["size_bytes"],
        size
    );
    fixture
        .expect(403, "HEAD", "/private-s3/a.mp4", Body::empty())
        .await;
    let private = fixture
        .request(
            "HEAD",
            "/private-s3/a.mp4",
            &[(
                "authorization",
                "Basic b2JqZWN0LXVzZXI6b2JqZWN0LXBhc3N3b3Jk",
            )],
            Body::empty(),
        )
        .await;
    assert_eq!(private.status(), 200);
    assert_eq!(private.headers()["cache-control"], "private, no-store");
    fixture.expect(200, "DELETE", path, Body::empty()).await;
    fixture.close().await;
}

fn tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_channel(fixture: &Fixture, protocol: &str, wanted: bool) {
    for _ in 0..90 {
        let value = fixture
            .json(200, "GET", &format!("/api/v3/{protocol}/channels"), None)
            .await;
        if value
            .as_array()
            .is_some_and(|list| list.is_empty() != wanted)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{protocol} channel readiness didn't reach {wanted}");
}

#[tokio::test]
#[ignore = "requires MediaMTX, provisioned by scripts/test-media-services.sh"]
async fn rtmp_srt_encrypted_ingest_playback_disconnect_and_flv() {
    let dir = TempDir::new().unwrap();
    let mediamtx = std::env::var("PHASE3_MEDIAMTX_BINARY").unwrap();
    let (cert, key) = certificate(&dir);
    let rtmp_tls = format!("127.0.0.1:{}", tcp_port());
    let rtmp = format!("127.0.0.1:{}", tcp_port());
    let srt = format!("127.0.0.1:{}", udp_port());
    let fixture = Fixture::open(
        &dir,
        &[
            ("CORE_MEDIA_TRANSPORT_BINARY", &mediamtx),
            ("CORE_RTMP_ENABLE", "true"),
            ("CORE_RTMP_ADDRESS", &rtmp),
            ("CORE_RTMP_ENABLE_TLS", "true"),
            ("CORE_RTMP_ADDRESS_TLS", &rtmp_tls),
            ("CORE_TLS_CERT_FILE", &cert),
            ("CORE_TLS_KEY_FILE", &key),
            ("CORE_RTMP_HTTPFLV_ENABLE", "true"),
            ("CORE_RTMP_TOKEN", "rtmp-fixture"),
            ("CORE_SRT_ENABLE", "true"),
            ("CORE_SRT_ADDRESS", &srt),
            ("CORE_SRT_PASSPHRASE", "srt-password-fixture"),
            ("CORE_SRT_TOKEN", "srt-fixture"),
        ],
    )
    .await;
    for wire in ["rtmp", "rtmps", "srt"] {
        let protocol = if wire == "srt" { "srt" } else { "rtmp" };
        let tls = if wire == "rtmps" {
            &["-tls_verify", "1", "-ca_file", cert.as_str()][..]
        } else {
            &[]
        };
        let (publisher, player, format) = if protocol == "rtmp" {
            let address = if wire == "rtmps" { &rtmp_tls } else { &rtmp };
            let uri = format!("{wire}://{address}/live/color?token=rtmp-fixture");
            (uri.clone(), uri, "flv")
        } else {
            let uri = |mode: &str| {
                format!(
                    "srt://{srt}?streamid={mode}:color:token=srt-fixture&mode=caller&passphrase=srt-password-fixture&pbkeylen=16"
                )
            };
            (
                uri("publish"),
                format!("{}&timeout=10000000", uri("read")),
                "mpegts",
            )
        };
        let publisher_started = std::time::Instant::now();
        let mut encoder = tokio::process::Command::new("ffmpeg");
        let mut child = encoder
            .kill_on_drop(true)
            .args([
                "-nostdin",
                "-v",
                "error",
                "-re",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x64:rate=10",
                "-t",
                "45",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-g",
                "10",
                "-f",
                format,
            ])
            .args(tls)
            .arg(&publisher)
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        wait_channel(&fixture, protocol, true).await;
        println!(
            "{wire} publisher launch to channel inventory: {:.2} s",
            publisher_started.elapsed().as_secs_f64()
        );
        if wire == "rtmp" {
            fixture
                .expect(403, "GET", "/rtmp/live/color", Body::empty())
                .await;
            let response = fixture
                .request(
                    "GET",
                    "/rtmp/live/color?token=rtmp-fixture",
                    &[],
                    Body::empty(),
                )
                .await;
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-type"], "video/x-flv");
            let mut data = response.into_body().into_data_stream();
            let first = futures_util::StreamExt::next(&mut data)
                .await
                .unwrap()
                .unwrap();
            assert!(first.starts_with(b"FLV"));
            drop(data);
        }
        let reader_started = std::time::Instant::now();
        let input_options = if wire == "srt" {
            &[
                "-f",
                "mpegts",
                "-probesize",
                "131072",
                "-analyzeduration",
                "1000000",
            ][..]
        } else {
            &[]
        };
        let status = tokio::time::timeout(
            Duration::from_secs(25),
            tokio::process::Command::new("ffmpeg")
                .kill_on_drop(true)
                .args(["-nostdin", "-v", "error"])
                .args(tls)
                .args(input_options)
                .args(["-i", &player, "-frames:v", "6", "-f", "null", "-"])
                .stderr(std::process::Stdio::inherit())
                .status(),
        )
        .await
        .unwrap_or_else(|_| panic!("{wire} playback exceeded its deadline"))
        .unwrap();
        assert!(status.success(), "{wire}");
        println!(
            "{wire} playback plus six-frame decode: {:.2} s",
            reader_started.elapsed().as_secs_f64()
        );
        let disconnect = if protocol == "rtmp" {
            "/api/v3/rtmp/disconnect?path=*"
        } else {
            "/api/v3/srt/disconnect"
        };
        fixture.json(200, "PUT", disconnect, None).await;
        wait_channel(&fixture, protocol, false).await;
        let _ = child.kill().await;
    }
    for (protocol, address, format) in [
        ("rtmp", "{rtmp,name=live/reconnected}", "flv"),
        ("srt", "{srt,name=reconnected}", "mpegts"),
    ] {
        let id = format!("{protocol}-job");
        let cfg = json!({
            "id":id,"autostart":true,
            "input":[{"id":"in","address":"testsrc2=size=64x64:rate=10","options":["-re","-f","lavfi"]}],
            "output":[{"id":"out","address":address,"options":["-c:v","libx264","-threads","1","-preset","ultrafast","-f",format]}]
        });
        fixture
            .json(200, "POST", "/api/v3/process", Some(cfg))
            .await;
        wait_channel(&fixture, protocol, true).await;
        fixture
            .json(200, "DELETE", &format!("/api/v3/process/{id}"), None)
            .await;
        wait_channel(&fixture, protocol, false).await;
    }

    fixture.close().await;
}

#[tokio::test]
async fn process_placeholder_publishes_to_memory_and_cleanup_preserves_other_processes() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(&dir, &[("CORE_STORAGE_MEMORY_MAX_SIZE_MBYTES", "4")]).await;
    let other = json!({
        "id":"owned", "input":[{"id":"src","address":"testsrc2=size=64x64:rate=10","options":["-f","lavfi"]}],
        "output":[{"id":"out","address":"{memfs}/protected.ts","options":["-t","0.3","-c:v","mpeg2video","-threads","1","-f","mpegts","-method","PUT"]}]
    });
    let source = json!({
        "id":"purged", "autostart":true, "input":other["input"],
        "output":[{"id":"out","address":"{memfs}/published.ts","options":["-t","0.3","-c:v","mpeg2video","-threads","1","-f","mpegts","-method","PUT"],
          "cleanup":[{"pattern":"{memfs}/*.ts","purge_on_delete":true}]}]
    });
    fixture
        .json(200, "POST", "/api/v3/process", Some(other))
        .await;
    fixture
        .expect(
            201,
            "PUT",
            "/api/v3/fs/mem/protected.ts",
            Body::from("keep"),
        )
        .await;
    fixture
        .json(200, "POST", "/api/v3/process", Some(source))
        .await;
    let published = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if fixture
                .request("HEAD", "/memfs/published.ts", &[], Body::empty())
                .await
                .status()
                == 200
            {
                break;
            }
            let report = fixture
                .json(200, "GET", "/api/v3/process/purged/report", None)
                .await;
            if let Some(run) = report["history"]
                .as_array()
                .and_then(|history| history.first())
            {
                assert_eq!(run["exit_state"], "finished", "{run}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if published.is_err() {
        let state = fixture
            .json(200, "GET", "/api/v3/process/purged/state", None)
            .await;
        let report = fixture
            .json(200, "GET", "/api/v3/process/purged/report", None)
            .await;
        panic!("FFmpeg did not publish to memory: state={state} report={report}");
    }
    fixture
        .json(200, "DELETE", "/api/v3/process/purged?purge=true", None)
        .await;
    fixture
        .expect(404, "HEAD", "/memfs/published.ts", Body::empty())
        .await;
    fixture
        .expect(200, "GET", "/memfs/protected.ts", Body::empty())
        .await;
    fixture.close().await;
}

#[tokio::test]
async fn restored_memory_process_waits_for_the_new_http_listener() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(&dir, &[("CORE_STORAGE_MEMORY_MAX_SIZE_MBYTES", "4")]).await;
    let cfg = json!({"id":"resume", "autostart":true,
        "input":[{"id":"in","address":"testsrc2=size=64x64:rate=10","options":["-f","lavfi"]}],
        "output":[{"id":"out","address":"{memfs}/resume.ts","options":["-t","0.2","-c:v","mpeg2video","-threads","1","-f","mpegts","-method","PUT"]}]
    });
    fixture
        .json(200, "POST", "/api/v3/process", Some(cfg))
        .await;
    fixture.close().await;
    let fixture = Fixture::open(&dir, &[("CORE_STORAGE_MEMORY_MAX_SIZE_MBYTES", "4")]).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if fixture
                .request("HEAD", "/memfs/resume.ts", &[], Body::empty())
                .await
                .status()
                == 200
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn dash_manifest_and_segments_share_one_viewer() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(&dir, &[("CORE_SESSIONS_MAX_SESSIONS", "1")]).await;
    fixture
        .expect(
            201,
            "PUT",
            "/api/v3/fs/disk/dash/manifest.mpd",
            Body::from("<MPD/>"),
        )
        .await;
    fixture
        .expect(
            201,
            "PUT",
            "/api/v3/fs/disk/dash/init.m4s",
            Body::from("init"),
        )
        .await;
    let (headers, _) = fixture
        .expect(200, "GET", "/dash/manifest.mpd", Body::empty())
        .await;
    let cookie = headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    for _ in 0..3 {
        let response = fixture
            .request(
                "GET",
                "/dash/init.m4s",
                &[("cookie", cookie)],
                Body::empty(),
            )
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(to_bytes(response.into_body(), 100).await.unwrap(), "init");
    }
    let sessions = fixture
        .json(200, "GET", "/api/v3/session/active?collectors=dash", None)
        .await;
    assert_eq!(sessions["dash"].as_array().unwrap().len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn extended_ffmpeg_playout_reports_unavailable_operations() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(&dir, &[]).await;
    let cfg = json!({"id":"play",
        "input":[{"id":"in","address":"testsrc2=size=64x64:rate=10","options":["-f","lavfi"]}],
        "output":[{"id":"out","address":"-","options":["-f","null"]}]
    });
    fixture
        .json(200, "POST", "/api/v3/process", Some(cfg))
        .await;
    fixture
        .expect(
            404,
            "GET",
            "/api/v3/process/play/playout/in/status",
            Body::empty(),
        )
        .await;
    fixture.close().await;

    let fixture = Fixture::open(&dir, &[("CORE_PLAYOUT_ENABLE", "true")]).await;
    for (method, route) in [
        ("GET", "status"),
        ("GET", "reopen"),
        ("GET", "keyframe/snapshot.png"),
        ("GET", "errorframe/encode"),
        ("PUT", "errorframe/error.png"),
        ("POST", "errorframe/error.png"),
        ("PUT", "stream"),
    ] {
        let path = format!("/api/v3/process/play/playout/in/{route}");
        let error = fixture.json(501, method, &path, None).await;
        assert_eq!(error["code"], 501);
    }
    fixture.close().await;
}

#[tokio::test]
#[ignore = "repeatable measurement, run with --ignored --nocapture"]
async fn benchmark_http_media() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(
        &dir,
        &[
            ("CORE_STORAGE_MEMORY_MAX_SIZE_MBYTES", "32"),
            ("CORE_SESSIONS_MAX_VIEWERS_PER_STREAM", "1024"),
        ],
    )
    .await;
    for backend in ["disk", "mem"] {
        fixture
            .expect(
                201,
                "PUT",
                &format!("/api/v3/fs/{backend}/benchmark.m4s"),
                Body::from(vec![42; 4 * 1024 * 1024]),
            )
            .await;
    }
    let manifest =
        "#EXTM3U\n#EXT-X-VERSION:7\n".to_owned() + &"#EXTINF:1,\nbenchmark.m4s\n".repeat(500);
    fixture
        .expect(
            201,
            "PUT",
            "/api/v3/fs/mem/benchmark.m3u8",
            Body::from(manifest),
        )
        .await;
    let client = reqwest::Client::new();
    for (label, path, range, loops) in [
        ("disk full", "/benchmark.m4s", None, 12),
        ("memory full", "/memfs/benchmark.m4s", None, 12),
        (
            "memory range",
            "/memfs/benchmark.m4s",
            Some("bytes=1048576-2097151"),
            24,
        ),
        ("HLS rewrite", "/memfs/benchmark.m3u8", None, 20),
    ] {
        let started = std::time::Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let client = client.clone();
            let url = format!("{}{path}", fixture.origin);
            tasks.spawn(async move {
                let mut bytes = 0;
                for _ in 0..loops {
                    let mut req = client.get(&url);
                    if let Some(range) = range {
                        req = req.header("Range", range);
                    }
                    let response = req.send().await.unwrap().error_for_status().unwrap();
                    bytes += response.bytes().await.unwrap().len();
                }
                bytes
            });
        }
        let mut transferred = 0;
        while let Some(result) = tasks.join_next().await {
            transferred += result.unwrap();
        }
        let elapsed = started.elapsed().as_secs_f64();
        println!(
            "{label}: {:.1} MiB/s, {:.0} req/s (8 viewers, {elapsed:.2}s)",
            transferred as f64 / elapsed / (1024.0 * 1024.0),
            f64::from(loops * 8) / elapsed
        );
    }
    for storage in ["disk", "mem"] {
        let mut latencies = Vec::new();
        for index in 0..24 {
            let path = format!("/api/v3/fs/{storage}/write-bench-{index}.ts");
            let start = std::time::Instant::now();
            fixture
                .expect(201, "PUT", &path, Body::from(vec![4; 4096]))
                .await;
            latencies.push(start.elapsed());
            fixture.expect(200, "DELETE", &path, Body::empty()).await;
        }
        latencies.sort_unstable();
        println!(
            "{storage} storage 4 KiB publish: p50 {:.2} ms, p95 {:.2} ms",
            latencies[12].as_secs_f64() * 1000.0,
            latencies[22].as_secs_f64() * 1000.0
        );
    }

    let url = format!("{}/memfs/benchmark.m4s", fixture.origin);
    let mut viewers = Vec::new();
    for _ in 0..4 {
        let client = client.clone();
        let url = url.clone();
        viewers.push(tokio::spawn(async move {
            let mut stream = client
                .get(url)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .bytes_stream();
            let mut received = 0;
            use futures_util::StreamExt;
            while let Some(chunk) = stream.next().await {
                received += chunk.unwrap().len();
                tokio::time::sleep(Duration::from_millis(8)).await;
            }
            received
        }));
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let start = std::time::Instant::now();
    let fast = client
        .get(&url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(fast.len(), 4 * 1024 * 1024);
    println!(
        "unslowed 4 MiB download with four paced viewers: {:.2} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    for viewer in viewers {
        assert_eq!(viewer.await.unwrap(), fast.len());
    }

    let status = tokio::fs::read_to_string("/proc/self/status")
        .await
        .unwrap();
    println!(
        "{}",
        status
            .lines()
            .find(|line| line.starts_with("VmHWM:"))
            .unwrap()
    );
    fixture.close().await;
}

fn certificate(dir: &TempDir) -> (String, String) {
    let cert = dir.path().join("fixture.crt");
    let key = dir.path().join("fixture.key");
    let status = std::process::Command::new("openssl")
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
            cert.to_str().unwrap(),
            "-keyout",
            key.to_str().unwrap(),
        ])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    (
        cert.into_os_string().into_string().unwrap(),
        key.into_os_string().into_string().unwrap(),
    )
}

#[tokio::test]
async fn https_uses_a_verified_local_certificate_for_media_requests() {
    let dir = TempDir::new().unwrap();
    let (cert, key) = certificate(&dir);
    let fixture = Fixture::open(
        &dir,
        &[
            ("CORE_TLS_ENABLE", "true"),
            ("CORE_API_AUTH_PASSWORD", "https-fixture-password"),
            ("CORE_API_AUTH_DISABLE_LOCALHOST", "true"),
            ("CORE_TLS_ADDRESS", "127.0.0.1:0"),
            ("CORE_TLS_CERT_FILE", &cert),
            ("CORE_TLS_KEY_FILE", &key),
        ],
    )
    .await;
    fixture
        .expect(
            201,
            "PUT",
            "/api/v3/fs/disk/secure.ts",
            Body::from("tls media"),
        )
        .await;
    let listener = fixture.app.bind_tls().await.unwrap().unwrap();
    let addr = axum::serve::Listener::local_addr(&listener).unwrap().0;
    let stop = tokio_util::sync::CancellationToken::new();
    let handle = tokio::spawn(restreamer_core::serve_http(
        listener,
        fixture.router.clone(),
        stop.clone(),
    ));
    let root = reqwest::Certificate::from_pem(&tokio::fs::read(cert).await.unwrap()).unwrap();
    let url = format!("https://{addr}/secure.ts");
    assert!(reqwest::Client::new().get(&url).send().await.is_err());
    let client = reqwest::Client::builder()
        .add_root_certificate(root)
        .build()
        .unwrap();
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap(), "tls media");
    let signed_in = client
        .post(format!("https://{addr}/api/login"))
        .header("Content-Type", "application/json")
        .body(json!({"username":"admin","password":"https-fixture-password"}).to_string())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let tokens: Value = signed_in.json().await.unwrap();
    assert!(
        tokens["access_token"]
            .as_str()
            .is_some_and(|token| !token.is_empty())
    );
    stop.cancel();
    handle.await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn http_viewer_capacity_and_paced_bytes() {
    let dir = TempDir::new().unwrap();
    let fixture = Fixture::open(
        &dir,
        &[
            ("CORE_SESSIONS_MAX_SESSIONS", "1"),
            ("CORE_SESSIONS_SESSION_BITRATE_KBIT", "8"),
        ],
    )
    .await;
    fixture
        .expect(
            201,
            "PUT",
            "/api/v3/fs/disk/item.ts",
            Body::from(vec![0; 300]),
        )
        .await;
    let start = std::time::Instant::now();
    fixture.expect(200, "GET", "/item.ts", Body::empty()).await;
    assert!(start.elapsed() >= Duration::from_millis(275));
    fixture.expect(507, "GET", "/item.ts", Body::empty()).await;
    fixture.close().await;
}
