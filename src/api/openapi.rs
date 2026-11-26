use axum::{
    Json,
    response::{Html, Redirect},
};
use serde_json::{Value, json};

pub async fn redirect() -> Redirect {
    Redirect::temporary("/api/swagger/index.html")
}

pub async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><title>Restreamer Core API</title>
<h1>Restreamer Core API</h1><p><a href="doc.json">OpenAPI document</a></p></html>"#,
    )
}

pub async fn document() -> Json<Value> {
    let mut spec = json!({
        "openapi": "3.1.0",
        "info": {"title":"Castvane Core", "version":env!("CARGO_PKG_VERSION")},
        "paths": {
            "/ping":{"get":{"summary":"Check liveness","responses":{"200":{"description":"Alive","content":{"text/plain":{"schema":{"const":"pong"}}}}}}},
            "/api":{"get":{"summary":"Discover node and available features","security":[{},{"bearer":[]}],"responses":{"200":{"description":"Discovery document","content":{"application/json":{"schema":{"type":"object","required":["app","auths","version","capabilities"]}}}},"401":error(),"403":error()}}},
            "/api/login":{"post":{"summary":"Authenticate the local administrator","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","required":["username","password"],"properties":{"username":{"type":"string"},"password":{"type":"string","writeOnly":true}}}}}},"responses":{"200":tokens(true),"403":error(),"429":error()}}},
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
    for (path, methods) in [
        ("/skills", &["get"][..]),
        ("/skills/reload", &["get"][..]),
        ("/process", &["get", "post"][..]),
        ("/process/{id}", &["get", "put", "delete"][..]),
        ("/process/{id}/config", &["get"][..]),
        ("/process/{id}/state", &["get"][..]),
        ("/process/{id}/report", &["get", "put"][..]),
        ("/process/{id}/command", &["put"][..]),
        ("/process/probe", &["post"][..]),
        ("/process/validate", &["post"][..]),
        ("/process/{id}/probe", &["get"][..]),
        ("/process/{id}/metadata", &["get"][..]),
        ("/process/{id}/metadata/{key}", &["get", "put"][..]),
        ("/metadata", &["get"][..]),
        ("/metadata/{key}", &["get", "put"][..]),
        ("/report/process", &["get"][..]),
    ] {
        let parameters: Vec<_> = ["id", "key"]
            .into_iter()
            .filter(|name| path.contains(&format!("{{{name}}}")))
            .map(|name| json!({"name":name,"in":"path","required":true,"schema":{"type":"string"}}))
            .collect();
        for method in methods {
            spec["paths"][format!("/api/v3{path}")][*method] = json!({
                "security":[{"bearer":[]}],"parameters":parameters,
                "responses":{"200":{"description":"Legacy v3 response"},"400":error(),"403":error(),"404":error(),"409":error(),"503":error()}
            });
        }
    }
    media_paths(&mut spec);
    operations_paths(&mut spec);
    Json(spec)
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
        let parameters: Vec<_> = ["name", "id", "kind"]
            .into_iter()
            .filter(|name| path.contains(&format!("{{{name}}}")))
            .map(|name| json!({"name":name,"in":"path","required":true,"schema":{"type":"string"}}))
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
                _ => None,
            };
            if let Some(schema) = schema {
                operation["requestBody"] = json!({"required":true,"content":{"application/json":
                    {"schema":{"$ref":format!("#/components/schemas/{schema}")}}}});
            }
            if path.starts_with("/api/v3/events") {
                operation["responses"]["200"]["content"] = json!({
                    "application/x-json-stream": {"schema":{"type":"string"}},
                    "text/event-stream":{"schema":{"type":"string"}}
                });
            }
            if path == "/metrics" {
                operation["responses"]["200"]["content"] =
                    json!({"text/plain":{"schema":{"type":"string"}}});
            }
            spec["paths"][path][*method] = operation;
        }
    }
    let schemas = &mut spec["components"]["schemas"];
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
