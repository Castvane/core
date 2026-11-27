use axum::{
    Json,
    http::header,
    response::{Html, IntoResponse, Redirect},
};
use serde_json::{Value, json};

pub async fn redirect() -> Redirect {
    Redirect::temporary("/api/swagger/index.html")
}

pub async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><title>Castvane Core API</title>
<h1>Castvane Core API</h1><p><a href="doc.json">OpenAPI document</a> · <a href="openapi.json">Download JSON</a></p></html>"#,
    )
}

pub async fn download() -> impl IntoResponse {
    (
        [(
            header::CONTENT_DISPOSITION,
            "attachment; filename=castvane-openapi.json",
        )],
        document().await,
    )
}

pub async fn document() -> Json<Value> {
    let mut spec = json!({
        "openapi": "3.1.0",
        "info": {"title":"Castvane Core", "version":env!("CARGO_PKG_VERSION")},
        "paths": {
            "/ping":{"get":{"summary":"Check liveness","responses":{"200":{"description":"Alive","content":{"text/plain":{"schema":{"const":"pong"}}}}}}},
            "/api":{"get":{"summary":"Discover node and available features","security":[{},{"bearer":[]}],"responses":{"200":{"description":"Discovery document","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Discovery"}}}},"401":error(),"403":error()}}},
            "/api/login":{"post":{"summary":"Authenticate a local identity","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","required":["username","password"],"properties":{"username":{"type":"string"},"password":{"type":"string","writeOnly":true}}}}}},"responses":{"200":tokens(true),"403":error(),"429":error()}}},
            "/api/login/refresh":{"get":{"summary":"Issue an access token using a refresh token","security":[{"bearer":[]}],"responses":{"200":tokens(false),"401":error(),"403":error()}}},
            "/api/v3/config":{
                "get":{"summary":"Read effective configuration","security":[{"bearer":[]}],"responses":{"200":{"description":"Configuration and timestamps","content":{"application/json":{"schema":{"type":"object","required":["config","created_at","updated_at","loaded_at","overrides"]}}}},"401":error(),"403":error()}},
                "put":{"summary":"Update a partial configuration","security":[{"bearer":[]}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","required":["version"],"properties":{"version":{"enum":[1,2,3]}}}}}},"responses":{"200":ok(),"400":error(),"401":error(),"403":error(),"405":error(),"409":{"description":"Configuration errors keyed by field","content":{"application/json":{"schema":{"type":"object","additionalProperties":{"type":"array","items":{"type":"string"}}}}}}}}
            },
            "/api/v3/config/reload":{"get":{"summary":"Reload configuration","security":[{"bearer":[]}],"responses":{"200":ok(),"401":error(),"403":error(),"404":error()}}}
        },
        "components":{
            "securitySchemes":{"bearer":{"type":"http","scheme":"bearer","bearerFormat":"JWT"}},
            "schemas":{"Error":{"type":"object","required":["code","message","details"],"properties":{"code":{"type":"integer"},"message":{"type":"string"},"details":{"type":"array","items":{"type":"string"}}}}}
        }
    });
    discovery_schemas(&mut spec);
    process_paths(&mut spec);
    media_paths(&mut spec);
    operations_paths(&mut spec);
    cluster_paths(&mut spec);
    documentation_paths(&mut spec);
    Json(spec)
}

fn discovery_schemas(spec: &mut Value) {
    let schemas = &mut spec["components"]["schemas"];
    schemas["Discovery"] = json!({
        "type":"object",
        "required":["app","auths","version","capabilities"],
        "properties":{
            "app":{"const":"datarhei-core"},
            "auths":{"type":"array","items":{"type":"string"}},
            "version":{"type":"object","required":["number"],"properties":{
                "number":{"type":"string"},
                "repository_commit":{"type":"string"},"repository_branch":{"type":"string"},
                "build_date":{"type":"string"},"compiler":{"type":"string"},"arch":{"type":"string"}
            }},
            "id":{"type":"string"},"name":{"type":"string"},
            "created_at":{"type":"string","format":"date-time"},
            "uptime_seconds":{"type":"integer","minimum":0},"resources":{"type":"object"},
            "capabilities":{"$ref":"#/components/schemas/DiscoveryCapabilities"}
        }
    });
    let mut capabilities = json!({
        "type":"object",
        "description":"Node feature availability. Management requests still require the relevant authorization. Check /readyz for runtime readiness.",
        "properties":{
            "schema_version":{"const":1},"product":{"const":"castvane"},
            "api_version":{"const":3},"api_prefix":{"const":crate::LEGACY_API_PREFIX},
            "configuration":{"type":"boolean"},"configuration_write":{"type":"boolean"},
            "process_engine":{"type":"boolean"},"local_login":{"type":"boolean"},
            "media":{"type":"boolean"},"clustering":{"type":"boolean"},
            "graphql":{"type":"boolean"},"playout":{"type":"boolean"},
            "transports":{"$ref":"#/components/schemas/DiscoveryTransports"}
        }
    });
    let mut transports = json!({
        "type":"object","properties":{
            "rtmp":{"type":"boolean"},"rtmps":{"type":"boolean"},
            "srt":{"type":"boolean"},"http_flv":{"type":"boolean"}
        }
    });
    for schema in [&mut capabilities, &mut transports] {
        schema["required"] = json!(
            schema["properties"]
                .as_object()
                .into_iter()
                .flat_map(|p| p.keys())
                .collect::<Vec<_>>()
        );
    }
    schemas["DiscoveryCapabilities"] = capabilities;
    schemas["DiscoveryTransports"] = transports;
}

fn documentation_paths(spec: &mut Value) {
    let paths = &mut spec["paths"];
    for (path, description, media_type) in [
        ("/api/swagger/index.html", "OpenAPI index", "text/html"),
        (
            "/api/swagger/doc.json",
            "OpenAPI document",
            "application/json",
        ),
        (
            "/api/swagger/openapi.json",
            "Download the OpenAPI document",
            "application/json",
        ),
        ("/api/graph", "GraphQL index", "text/html"),
        (
            "/api/graph/schema.graphql",
            "Download the GraphQL schema",
            "text/plain",
        ),
    ] {
        paths[path]["get"] = json!({"summary":description, "security":[{"bearer":[]}],
            "responses":{"200":{"description":description, "content":{(media_type): {}}},
                "401":error(),"403":error()}});
    }
    paths["/api/graph/query"]["post"] = json!({
        "summary":"Execute a bounded GraphQL query",
        "description":"Requires an access token when API authentication is enabled. Process fields enforce resource policy. Probe also requires process write permission and an editable process.",
        "security":[{"bearer":[]}],
        "requestBody":{"required":true,"content":{"application/json":{
            "schema":{"type":"object","required":["query"],"properties":{
                "query":{"type":"string"},"variables":{"type":["object","null"]},
                "operationName":{"type":["string","null"]}
            }},"example":{"query":"{ ping }"}
        }}},
        "responses":{"200":{"description":"Execution result, including any GraphQL validation or field errors",
            "content":{"application/json":{"schema":{"type":"object","properties":{
                "data":{"type":["object","null"]},"errors":{"type":"array","items":{"type":"object","required":["message"],"properties":{"message":{"type":"string"},"path":{"type":"array","items":{"type":["integer","string"]}}}}}
            }}}}},
            "400":{"description":"Invalid JSON request"},"401":error(),"403":error(),
            "408":{"description":"Execution deadline exceeded"},"413":{"description":"Request exceeds 128 KiB"}}
    });
}

fn process_paths(spec: &mut Value) {
    let report_search = &[
        "id",
        "reference",
        "idpattern",
        "refpattern",
        "ownerpattern",
        "domainpattern",
        "zonepattern",
        "state",
        "from",
        "to",
        "offset",
        "limit",
    ][..];
    let list_filters = &[
        "id",
        "reference",
        "idpattern",
        "refpattern",
        "ownerpattern",
        "domainpattern",
        "zonepattern",
        "filter",
        "offset",
        "limit",
    ][..];
    for (path, method, query, request, response) in [
        ("/skills", "get", &[][..], "", "Skills"),
        ("/skills/reload", "get", &[][..], "", "Skills"),
        ("/process", "get", list_filters, "", "Processes"),
        (
            "/process",
            "post",
            &[][..],
            "ProcessConfiguration",
            "ProcessConfiguration",
        ),
        (
            "/process/{id}",
            "get",
            &["domain", "filter"][..],
            "",
            "Process",
        ),
        (
            "/process/{id}",
            "put",
            &["domain", "force"][..],
            "ProcessConfiguration",
            "ProcessConfiguration",
        ),
        (
            "/process/{id}",
            "delete",
            &["domain", "purge"][..],
            "",
            "OK",
        ),
        (
            "/process/{id}/config",
            "get",
            &["domain"][..],
            "",
            "ProcessConfiguration",
        ),
        (
            "/process/{id}/state",
            "get",
            &["domain"][..],
            "",
            "ProcessState",
        ),
        (
            "/process/{id}/report",
            "get",
            &["domain", "created_at", "exited_at"][..],
            "",
            "ProcessReport",
        ),
        (
            "/process/{id}/report",
            "put",
            &["domain"][..],
            "ReportImport",
            "OK",
        ),
        (
            "/process/{id}/command",
            "put",
            &["domain"][..],
            "ProcessCommand",
            "OK",
        ),
        (
            "/process/probe",
            "post",
            &[][..],
            "ProcessConfiguration",
            "Probe",
        ),
        (
            "/process/validate",
            "post",
            &[][..],
            "ProcessConfiguration",
            "ProcessConfiguration",
        ),
        ("/process/{id}/probe", "get", &["domain"][..], "", "Probe"),
        (
            "/process/{id}/metadata",
            "get",
            &["domain"][..],
            "",
            "Metadata",
        ),
        (
            "/process/{id}/metadata/{key}",
            "get",
            &["domain"][..],
            "",
            "Document",
        ),
        (
            "/process/{id}/metadata/{key}",
            "put",
            &["domain"][..],
            "Document",
            "Document",
        ),
        ("/metadata", "get", &[][..], "", "Metadata"),
        ("/metadata/{key}", "get", &[][..], "", "Document"),
        ("/metadata/{key}", "put", &[][..], "Document", "Document"),
        ("/report/process", "get", report_search, "", "ReportSearch"),
    ] {
        let parameters: Vec<_> = ["id", "key"]
            .into_iter()
            .filter(|name| path.contains(&format!("{{{name}}}")))
            .map(|name| json!({"name":name,"in":"path","required":true,"schema":{"type":"string"}}))
            .chain(query.iter().map(|name| {
                let schema = match *name {
                    "offset" | "limit" => json!({"type":"integer","minimum":0}),
                    "created_at" | "exited_at" | "from" | "to" => {
                        json!({"type":"integer","format":"int64"})
                    }
                    "force" => json!({"type":"string","enum":["restart"]}),
                    "purge" => json!({"type":"boolean","default":true}),
                    _ => json!({"type":"string"}),
                };
                json!({"name":name,"in":"query","schema":schema})
            }))
            .collect();
        let mut operation = json!({
            "tags":["Process engine"], "security":[{"bearer":[]}], "parameters":parameters,
            "responses":{"200":if response == "OK" { ok() } else { json!({
                "description":"Legacy v3 response", "content":schema_content(response)
            }) },"400":error(),"401":error(),"403":error(),"404":error(),"409":error(),"503":error()}
        });
        if !request.is_empty() {
            operation["requestBody"] = json!({"required":true,"content":schema_content(request)});
        }
        if path == "/process" && method == "get" {
            operation["responses"]["413"] = error();
        }
        spec["paths"][format!("/api/v3{path}")][method] = operation;
    }
    let schemas = &mut spec["components"]["schemas"];
    schemas["Document"] = json!({});
    schemas["Metadata"] = json!({"type":["object","null"]});
    schemas["ProcessEndpoint"] = json!({"type":"object","properties":{
        "id":{"type":"string"},"address":{"type":"string"},
        "options":{"type":["array","null"],"items":{"type":"string"}},
        "cleanup":{"type":["array","null"],"items":{"type":"object"}}
    }});
    schemas["ProcessConfiguration"] = json!({"type":"object","properties":{
        "id":{"type":"string"},"owner":{"type":"string"},"domain":{"type":"string"},"zone":{"type":"string"},
        "binary":{"type":"string"},"type":{"const":"ffmpeg"},"reference":{"type":"string"},
        "input":{"type":["array","null"],"items":{"$ref":"#/components/schemas/ProcessEndpoint"}},
        "output":{"type":["array","null"],"items":{"$ref":"#/components/schemas/ProcessEndpoint"}},
        "options":{"type":["array","null"],"items":{"type":"string"}},
        "reconnect":{"type":"boolean"},"autostart":{"type":"boolean"},
        "reconnect_delay_seconds":{"type":"integer","minimum":0},"reconnect_max_retries":{"type":"integer","minimum":0},
        "stale_timeout_seconds":{"type":"integer","minimum":0},"runtime_duration_seconds":{"type":"integer","minimum":0},
        "scheduler":{"type":"string","description":"Cron expression or RFC3339 timestamp"},
        "limits":{"type":"object"},"metadata":{"type":"object"},
        "log_patterns":{"type":["array","null"],"items":{"type":"string"}}
    }});
    schemas["ProcessCommand"] = json!({"type":"object","required":["command"],"properties":{
        "command":{"enum":["start","stop","reload","restart"]},"reason":{"type":"string"}
    }});
    schemas["ProcessState"] = json!({"type":"object","required":["order","exec","progress","resources"],"properties":{
        "order":{"enum":["start","stop"]},"exec":{"enum":["finished","starting","running","finishing","failed","killed"]},
        "progress":{"$ref":"#/components/schemas/ProcessProgress"},"resources":{"type":"object"},"pid":{"type":"integer"},
        "limit_mode":{"enum":["hard","soft","none"]}
    }});
    schemas["ProcessProgress"] = json!({"type":"object","properties":{
        "started":{"type":"boolean"},"frame":{"type":"integer"},"packet":{"type":"integer"},
        "fps":{"type":"number"},"q":{"type":"number"},"size_kb":{"type":"integer"},
        "time":{"type":"number","description":"Output timestamp in seconds"},
        "bitrate_kbit":{"type":"number"},"speed":{"type":"number"},
        "drop":{"type":"integer"},"dup":{"type":"integer"},
        "inputs":{"type":"array","items":{"$ref":"#/components/schemas/ProcessStreamProgress"}},
        "outputs":{"type":"array","items":{"$ref":"#/components/schemas/ProcessStreamProgress"}},
        "mapping":{"type":"object"}
    }});
    schemas["ProcessStreamProgress"] = json!({"type":"object","properties":{
        "id":{"type":"string"},"address":{"type":"string"},"index":{"type":"integer"},"stream":{"type":"integer"},
        "frame":{"type":"integer"},"keyframe":{"type":"integer"},"packet":{"type":"integer"},
        "size_kb":{"type":"integer"},"extradata_size_bytes":{"type":"integer"},
        "fps":{"type":"number"},"framerate":{"type":"object","properties":{
            "min":{"type":"number"},"max":{"type":"number"},"avg":{"type":"number"}
        }}
    }});
    schemas["ProcessReport"] = json!({"type":"object","required":["history"],"properties":{
        "history":{"type":"array","items":{"type":"object"}}
    }});
    schemas["ReportImport"] = json!({"type":"object","properties":{
        "history":{"type":"array","items":{"type":"object"}}
    }});
    schemas["Process"] = json!({"type":"object","required":["id","type"],"properties":{
        "id":{"type":"string"},"domain":{"type":"string"},"type":{"const":"ffmpeg"},
        "config":{"$ref":"#/components/schemas/ProcessConfiguration"},
        "state":{"$ref":"#/components/schemas/ProcessState"},
        "report":{"$ref":"#/components/schemas/ProcessReport"},
        "metadata":{"type":"object"}
    }});
    schemas["Processes"] = json!({"type":"array","items":{"$ref":"#/components/schemas/Process"}});
    schemas["ReportSearch"] = json!({"type":"array","items":{"type":"object","properties":{
        "id":{"type":"string"},"domain":{"type":"string"},"reference":{"type":"string"},
        "exit_state":{"type":"string"},"created_at":{"type":"integer"},"exited_at":{"type":"integer"}
    }}});
    schemas["Probe"] = json!({"type":"object","properties":{
        "streams":{"type":"array","items":{"type":"object"}},"log":{"type":"array","items":{"type":"string"}}
    }});
    schemas["Skills"] = json!({"type":"object","properties":{
        "ffmpeg":{"type":"object","properties":{"version":{"type":"string"},"configuration":{"type":"string"}}},
        "ffprobe_available":{"type":"boolean"},"ffprobe_version":{"type":["string","null"]},"resource_monitor":{"type":"object"}
    }});
}

fn schema_content(schema: &str) -> Value {
    json!({"application/json":{"schema":{"$ref":format!("#/components/schemas/{schema}")}}})
}

fn media_paths(spec: &mut Value) {
    let listing = &[
        "glob",
        "pattern",
        "size_min",
        "size_max",
        "lastmod_start",
        "lastmod_end",
        "sort",
        "order",
    ][..];
    for (path, methods, query, body) in [
        ("/fs", &["get"][..], &["details"][..], ""),
        ("/fs", &["put"][..], &[][..], "StorageOperation"),
        ("/fs/{storage}", &["get", "delete"][..], listing, ""),
        (
            "/fs/{storage}/{path}",
            &["get", "head", "delete"][..],
            &[][..],
            "",
        ),
        ("/fs/{storage}/{path}", &["put"][..], &[][..], "binary"),
        ("/rtmp", &["get"][..], &[][..], ""),
        ("/rtmp/channels", &["get"][..], &[][..], ""),
        ("/rtmp/disconnect", &["put"][..], &["path"][..], ""),
        ("/srt", &["get"][..], &[][..], ""),
        ("/srt/channels", &["get"][..], &[][..], ""),
        ("/srt/disconnect", &["put"][..], &[][..], ""),
        ("/session", &["get"][..], &["collectors"][..], ""),
        ("/session/active", &["get"][..], &["collectors"][..], ""),
        (
            "/session/token/{username}",
            &["put"][..],
            &[][..],
            "ViewerTokens",
        ),
        ("/alias", &["get"][..], &[][..], ""),
        ("/alias", &["post"][..], &[][..], "Alias"),
        ("/alias", &["put"][..], &[][..], "Aliases"),
        ("/alias/rule", &["get", "delete"][..], &["pattern"][..], ""),
        ("/alias/rule", &["put"][..], &["pattern"][..], "Alias"),
        ("/widget/process/{id}", &["get"][..], &["domain"][..], ""),
        (
            "/process/{id}/playout/{input}/status",
            &["get"][..],
            &["domain"][..],
            "",
        ),
        (
            "/process/{id}/playout/{input}/reopen",
            &["get"][..],
            &["domain"][..],
            "",
        ),
        (
            "/process/{id}/playout/{input}/stream",
            &["put"][..],
            &["domain"][..],
            "",
        ),
        (
            "/process/{id}/playout/{input}/keyframe/{name}",
            &["get"][..],
            &["domain"][..],
            "",
        ),
        (
            "/process/{id}/playout/{input}/errorframe/encode",
            &["get"][..],
            &["domain"][..],
            "",
        ),
        (
            "/process/{id}/playout/{input}/errorframe/{name}",
            &["put", "post"][..],
            &["domain"][..],
            "",
        ),
    ] {
        let parameters = ["storage", "path", "id", "input", "name", "username"]
            .into_iter()
            .filter(|name| path.contains(&format!("{{{name}}}")))
            .map(|name| json!({"name":name,"in":"path","required":true,"schema":{"type":"string"}}))
            .chain(query.iter().map(|name| {
                json!({"name":name,"in":"query","required":path=="/alias/rule",
                "schema":{"type":"string"}})
            }))
            .collect::<Vec<_>>();
        for method in methods {
            let mut operation = json!({
                "tags":["Media and storage"],
                "security":if path.starts_with("/widget") { json!([]) } else { json!([{"bearer":[]}]) },
                "parameters":parameters,
                "responses":{"200":{"description":"Legacy media response"},"400":error(),
                    "401":error(),"403":error(),"404":error(),"503":error()}
            });
            if path.ends_with("/{path}") {
                if matches!(*method, "get" | "head") {
                    operation["responses"]["206"] = json!({"description":"Partial content"});
                    operation["responses"]["304"] = json!({"description":"Not modified"});
                    operation["responses"]["416"] = json!({"description":"Range not satisfiable"});
                } else if *method == "put" {
                    operation["responses"]["201"] = json!({"description":"Created"});
                    operation["responses"]["204"] = json!({"description":"Replaced"});
                    operation["responses"]["507"] = error();
                }
            }
            if path.contains("/playout/") {
                operation["description"] =
                    json!("Requires the Restreamer FFmpeg control extension");
                operation["responses"]["501"] = error();
            }
            if !body.is_empty() {
                let content = if body == "binary" {
                    json!({"application/octet-stream":{"schema":{"type":"string","format":"binary"}}})
                } else {
                    json!({"application/json":{"schema":{"$ref":format!("#/components/schemas/{body}")}}})
                };
                operation["requestBody"] = json!({"required":true,"content":content});
            }
            spec["paths"][format!("/api/v3{path}")][*method] = operation;
        }
    }
    let schemas = &mut spec["components"]["schemas"];
    schemas["Alias"] = json!({"type":"object","required":["pattern","target"],
        "properties":{"pattern":{"type":"string"},"target":{"type":"string"}}});
    schemas["Aliases"] = json!({"type":"array","items":{"$ref":"#/components/schemas/Alias"}});
    schemas["ViewerTokens"] = json!({"type":"array","items":{"type":"object",
        "properties":{"match":{"type":"string"},"remote":{"type":"array","items":{"type":"string"}},"ttl_sec":{"type":"integer"},"extra":{},
                      "token":{"type":"string","readOnly":true}}}});
    schemas["StorageOperation"] = json!({"type":"object","required":["source","target","operation"],
        "properties":{"operation":{"enum":["copy","move"]},"source":{"type":"string"},
                      "target":{"type":"string"},"bandwidth_limit_kbit":{"type":"integer","minimum":0}}});
}

fn operations_paths(spec: &mut Value) {
    for (path, methods) in [
        ("/healthz", &["get"][..]),
        ("/readyz", &["get"][..]),
        ("/metrics", &["get"][..]),
        ("/api/v3/iam/user", &["get", "post"][..]),
        ("/api/v3/iam/user/{name}", &["get", "put", "delete"][..]),
        ("/api/v3/iam/user/{name}/policy", &["put"][..]),
        ("/api/v3/iam/user/{name}/revoke", &["put"][..]),
        ("/api/v3/iam/signing-key/rotate", &["put"][..]),
        ("/api/v3/zone", &["get", "post"][..]),
        ("/api/v3/zone/{id}", &["get", "put", "delete"][..]),
        ("/api/v3/metrics", &["get", "post"][..]),
        ("/api/v3/log", &["get"][..]),
        ("/api/v3/events", &["post"][..]),
        ("/api/v3/events/log", &["post"][..]),
        ("/api/v3/events/process", &["post"][..]),
        ("/api/v3/events/media/{kind}", &["post"][..]),
        ("/api/v3/resources", &["get"][..]),
        ("/api/v3/audit", &["get"][..]),
    ] {
        let query = match path {
            "/api/v3/log" => &["format", "component"][..],
            "/api/v3/events/media/{kind}" => &["glob"][..],
            "/api/v3/audit" => &["after"][..],
            _ => &[],
        };
        let parameters: Vec<_> = ["name", "id", "kind"]
            .into_iter()
            .filter(|name| path.contains(&format!("{{{name}}}")))
            .map(|name| json!({"name":name,"in":"path","required":true,"schema":{"type":"string"}}))
            .chain(query.iter().map(|name| {
                json!({"name":name,"in":"query","schema":{"type":
                    if *name == "after" {"integer"} else {"string"}
                }})
            }))
            .collect();
        for method in methods {
            let mut operation = json!({
                "tags":["Operations and IAM"],"parameters":parameters,
                "security":if matches!(path,"/healthz"|"/readyz") {json!([])} else {json!([{"bearer":[]}])},
                "responses":{"200":{"description":"Operation completed"},
                    "400":error(),"401":error(),"403":error(),"404":error(),"409":error(),"503":error()}
            });
            let schema = match (path, *method) {
                ("/api/v3/iam/user", "post") | ("/api/v3/iam/user/{name}", "put") => {
                    Some("IdentityWrite")
                }
                ("/api/v3/iam/user/{name}/policy", "put") => Some("IdentityPolicies"),
                ("/api/v3/zone", "post") | ("/api/v3/zone/{id}", "put") => Some("ZoneWrite"),
                ("/api/v3/metrics", "post") => Some("MetricsQuery"),
                ("/api/v3/events" | "/api/v3/events/log", "post") => Some("LogEventFilters"),
                ("/api/v3/events/process", "post") => Some("ProcessEventFilters"),
                _ => None,
            };
            if let Some(schema) = schema {
                operation["requestBody"] = json!({"required":!path.starts_with("/api/v3/events"),"content":schema_content(schema)});
            }
            if path.starts_with("/api/v3/events") {
                operation["responses"]["200"]["content"] =
                    json!({"application/x-json-stream":{"schema":{"type":"string"}}});
                if matches!(path, "/api/v3/events" | "/api/v3/events/log") {
                    operation["responses"]["200"]["content"]["text/event-stream"] =
                        json!({"schema":{"type":"string"}});
                }
            }
            if path == "/metrics" {
                operation["responses"]["200"]["content"] =
                    json!({"text/plain":{"schema":{"type":"string"}}});
            }
            spec["paths"][path][*method] = operation;
        }
    }
    let schemas = &mut spec["components"]["schemas"];
    for (name, fields) in [
        (
            "LogEventFilters",
            &["event", "message", "level", "caller", "core_id"][..],
        ),
        (
            "ProcessEventFilters",
            &["pid", "domain", "type", "core_id"][..],
        ),
    ] {
        let mut properties = serde_json::Map::new();
        for field in fields {
            properties.insert((*field).to_owned(), json!({"type":"string"}));
        }
        if name == "LogEventFilters" {
            properties.insert(
                "data".into(),
                json!({"type":"object","additionalProperties":{"type":"string"}}),
            );
        }
        schemas[name] = json!({"type":"object","properties":{"filters":{"type":"array","maxItems":32,
            "items":{"type":"object","properties":properties}
        }}});
    }
    schemas["IdentityWrite"] = json!({"type":"object","properties":{
        "name":{"type":"string"},"alias":{"type":"string"},"superuser":{"type":"boolean"},"active":{"type":"boolean"},
        "policies":{"$ref":"#/components/schemas/IdentityPolicies"},
        "auth":{"type":"object","writeOnly":true,"properties":{"api":{"type":"object",
            "properties":{"userpass":{"type":"string","writeOnly":true,"format":"password"}}}}}
    }});
    schemas["IdentityPolicies"] = json!({"type":"array","maxItems":64,"items":{"type":"object",
    "required":["domain","types","resource","actions"],"properties":{
        "domain":{"type":"string"},"types":{"type":"array","items":{"type":"string"}},
        "resource":{"type":"string"},"actions":{"type":"array","items":{"type":"string"}},
        "effect":{"enum":["allow","deny"],"default":"allow"}
    }}});
    schemas["ZoneWrite"] = json!({"type":"object","properties":{"id":{"type":"string"},
        "limits":{"type":"object","additionalProperties":{"type":"number","minimum":0}}}});
    schemas["MetricsQuery"] = json!({"type":"object","properties":{
        "timerange_sec":{"type":"integer","minimum":0},"interval_sec":{"type":"integer","minimum":0},
        "metrics":{"type":"array","maxItems":128,"items":{"type":"object","required":["name"],
            "properties":{"name":{"type":"string"},"labels":{"type":"object","additionalProperties":{"type":"string"}}}}}
    }});
}

fn cluster_paths(spec: &mut Value) {
    for (suffix, methods) in [
        ("", &["get"][..]),
        ("/healthy", &["get"][..]),
        ("/snapshot", &["get"][..]),
        ("/node", &["get"][..]),
        ("/node/{id}", &["get", "post"][..]),
        ("/node/{id}/version", &["get"][..]),
        ("/node/{id}/files", &["get"][..]),
        ("/node/{id}/process", &["get"][..]),
        ("/fs/{storage}", &["get"][..]),
        ("/node/{id}/state", &["get", "put"][..]),
        ("/node/{id}/fs", &["put"][..]),
        ("/node/{id}/fs/{storage}", &["get"][..]),
        (
            "/node/{id}/fs/{storage}/{path}",
            &["get", "head", "put", "delete"][..],
        ),
        ("/membership", &["put"][..]),
        ("/transfer/{id}", &["put"][..]),
        ("/leave", &["put"][..]),
        ("/deployments", &["get"][..]),
        ("/reallocation", &["put"][..]),
        ("/process", &["get", "post"][..]),
        ("/process/{id}", &["get", "put", "delete"][..]),
        ("/process/{id}/config", &["get"][..]),
        ("/process/{id}/command", &["put"][..]),
        ("/process/{id}/state", &["get"][..]),
        ("/process/{id}/playback", &["get"][..]),
        ("/process/{id}/report", &["get"][..]),
        ("/process/{id}/probe", &["get"][..]),
        ("/process/probe", &["post"][..]),
        ("/process/{id}/metadata", &["get"][..]),
        ("/process/{id}/metadata/{key}", &["get", "put"][..]),
        ("/db/kv", &["get"][..]),
        ("/db/kv/{key}", &["get", "put", "delete"][..]),
        ("/db/locks", &["get"][..]),
        ("/db/locks/{key}", &["put", "delete"][..]),
        ("/iam/user", &["get", "post"][..]),
        ("/iam/user/{name}", &["get", "put", "delete"][..]),
        ("/iam/user/{name}/policy", &["put"][..]),
        ("/iam/policies", &["get"][..]),
        ("/zone", &["get", "post"][..]),
        ("/zone/{id}", &["get", "put", "delete"][..]),
        ("/alias", &["get", "post", "put"][..]),
        ("/alias/rule", &["get", "put", "delete"][..]),
    ] {
        let parameters: Vec<_> = ["id", "key", "name", "storage", "path"]
            .into_iter()
            .filter(|name| suffix.contains(&format!("{{{name}}}")))
            .map(|name| json!({"name":name,"in":"path","required":true,"schema":{"type":"string"}}))
            .collect();
        for method in methods {
            spec["paths"][format!("/api/v3/cluster{suffix}")][*method] = json!({
                "tags":["Cluster"],"security":[{"bearer":[]}],"parameters":parameters,
                "description":"Administrator operations on an enabled Castvane cluster",
                "responses":{"200":{"description":"Success"},"400":error(),"401":error(),"403":error(),
                    "404":error(),"409":error(),"503":error()}
            });
        }
    }
    spec["components"]["schemas"]["ClusterNodeVersion"] = json!({
        "type":"object",
        "required":["number","repository_commit","repository_branch","build_date","arch","compiler"],
        "properties":{
            "number":{"type":"string"}, "repository_commit":{"type":"string"},
            "repository_branch":{"type":"string"}, "build_date":{"type":"string"},
            "arch":{"type":"string"}, "compiler":{"type":"string"}
        }
    });
    let version = &mut spec["paths"]["/api/v3/cluster/node/{id}/version"]["get"];
    version["description"] = json!(
        "Read the actual build metadata from a current member. Requires an administrator on the leader with quorum."
    );
    version["parameters"][0]["schema"] = json!({"type":"integer","minimum":1});
    version["responses"]["200"] = json!({
        "description":"Node build metadata",
        "content":schema_content("ClusterNodeVersion")
    });

    let transfer = &mut spec["paths"]["/api/v3/cluster/transfer/{id}"]["put"];
    transfer["description"] = json!(
        "Transfer leadership to a current voter. Requires the leader, an administrator, quorum and a writable API."
    );
    transfer["parameters"][0]["schema"] = json!({"type":"integer","minimum":1});
    transfer["responses"]["200"] = ok();
    transfer["responses"]["405"] = error();

    let leave = &mut spec["paths"]["/api/v3/cluster/leave"]["put"];
    leave["description"] = json!(
        "Remove a drained voter or learner with no assignments. Requires the leader, an administrator, quorum and a writable API. Transfer leadership before removing the current leader."
    );
    leave["requestBody"] = json!({
        "content":{"application/json":{"schema":{"type":"object",
            "required":["id"],"properties":{"id":{"type":"string","pattern":"^[0-9]+$"}}
        },"example":{"id":"4"}}}
    });
    leave["responses"]["200"] = ok();
    leave["responses"]["405"] = error();
    leave["responses"]["413"] = error();

    let node_files = &mut spec["paths"]["/api/v3/cluster/node/{id}/files"]["get"];
    node_files["description"] = json!(
        "List paths by mounted storage on a reachable member. Requires an administrator on the leader with quorum."
    );
    node_files["parameters"][0]["schema"] = json!({"type":"integer","minimum":1});
    node_files["responses"]["200"] = json!({
        "description":"Node file inventory",
        "content":{"application/json":{"schema":{"type":"object","required":["last_update","files"],
            "properties":{"last_update":{"type":"integer","minimum":0},
                "files":{"type":"object","additionalProperties":{"type":"array","items":{"type":"string"}}}}
        }}}
    });
    node_files["responses"]["507"] = error();

    let processes = &mut spec["paths"]["/api/v3/cluster/node/{id}/process"]["get"];
    processes["description"] = json!(
        "List process instances on a reachable member. Requires an administrator on the leader with quorum."
    );
    processes["parameters"][0]["schema"] = json!({"type":"integer","minimum":1});
    if let Some(parameters) = processes["parameters"].as_array_mut() {
        for name in [
            "filter",
            "id",
            "reference",
            "idpattern",
            "refpattern",
            "ownerpattern",
            "domainpattern",
            "zonepattern",
        ] {
            parameters.push(json!({"name":name,"in":"query","schema":{"type":"string"}}));
        }
        for name in ["offset", "limit"] {
            parameters
                .push(json!({"name":name,"in":"query","schema":{"type":"integer","minimum":0}}));
        }
    }
    processes["responses"]["200"] = json!({
        "description":"Node process inventory",
        "content":schema_content("Processes")
    });
    processes["responses"]["507"] = error();

    let operation = &mut spec["paths"]["/api/v3/cluster/node/{id}/fs"]["put"];
    operation["description"] = json!(
        "Copy or move between mounted filesystems on one member. Requires an administrator on the current leader with quorum and a writable API."
    );
    operation["parameters"][0]["schema"] = json!({"type":"integer","minimum":1});
    operation["requestBody"] =
        json!({"required":true,"content":schema_content("StorageOperation")});
    operation["responses"]["200"] = json!({
        "description":"Operation completed",
        "content":{"application/json":{"schema":{"type":"null"}}}
    });
    for status in ["405", "408", "413", "507"] {
        operation["responses"][status] = error();
    }

    let files = &mut spec["paths"]["/api/v3/cluster/fs/{storage}"]["get"];
    files["description"] = json!(
        "List matching files across reachable members. Requires an administrator on the leader with quorum. Identical names on separate nodes are retained."
    );
    files["parameters"] = json!([
        {"name":"storage","in":"path","required":true,"schema":{"type":"string"}},
        {"name":"glob","in":"query","schema":{"type":"string"}},
        {"name":"sort","in":"query","schema":{"enum":["none","name","size","lastmod"],"default":"none"}},
        {"name":"order","in":"query","schema":{"enum":["asc","desc"],"default":"asc"}}
    ]);
    files["responses"]["200"] = json!({
        "description":"Files from reachable nodes",
        "headers":{"X-Castvane-Unavailable-Nodes":{"schema":{"type":"integer","minimum":0}}},
        "content":{"application/json":{"schema":{"type":"array","items":{"type":"object",
            "required":["name","size_bytes","last_modified"],"properties":{
                "name":{"type":"string"},"size_bytes":{"type":"integer","minimum":0},
                "last_modified":{"type":"integer","minimum":0}}
        }}}}
    });
    files["responses"]["507"] = error();
}

fn error() -> Value {
    json!({"description":"Request rejected","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}})
}

fn ok() -> Value {
    json!({"description":"Success","content":{"application/json":{"schema":{"const":"OK"}}}})
}

fn tokens(refresh: bool) -> Value {
    let mut properties = json!({"access_token":{"type":"string"}});
    let mut required = vec!["access_token"];
    if refresh {
        properties["refresh_token"] = json!({"type":"string"});
        required.push("refresh_token");
    }
    json!({"description":"JWT response","content":{"application/json":{"schema":{"type":"object","properties":properties,"required":required}}}})
}
