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
        "info": {"title":"Restreamer Core", "version":env!("CARGO_PKG_VERSION")},
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
    Json(spec)
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
