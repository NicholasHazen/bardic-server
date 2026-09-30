//! Test support: a contract-conformance checker and an in-process test server.
#![allow(dead_code)]

pub mod breeze;
pub mod epub;

use bardic_server::{
    app::{spawn, Running},
    clock::{Clock, FakeClock},
    config::Config,
};
use jsonschema::Draft;
use reqwest::Method;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};
use tempfile::TempDir;

// ---------------------------------------------------------------- contract

/// The OpenAPI document, used to validate every response a test receives.
/// Undeclared fields, missing required fields, wrong types and undocumented
/// statuses all fail. Fix the code or the contract, never this checker.
pub struct Contract {
    doc: Value,
}

impl Contract {
    pub fn load() -> Self {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/contract/openapi.yaml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let doc: Value = serde_yaml::from_str(&text).expect("contract is valid YAML");
        let mut c = Contract { doc };
        c.strictify_components();
        c
    }

    pub fn operation_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for (_, item) in self.doc["paths"].as_object().expect("paths") {
            for (method, op) in item.as_object().expect("path item") {
                if ["get", "post", "put", "patch", "delete"].contains(&method.as_str()) {
                    ids.push(op["operationId"].as_str().expect("operationId").to_string());
                }
            }
        }
        ids
    }

    pub fn version(&self) -> &str {
        self.doc["info"]["version"]
            .as_str()
            .expect("contract version")
    }

    fn strictify_components(&mut self) {
        if let Some(schemas) = self.doc["components"]["schemas"].as_object_mut() {
            for (_, s) in schemas.iter_mut() {
                strict_schema(s, false);
            }
        }
    }

    fn resolve<'a>(&'a self, v: &'a Value) -> &'a Value {
        match v.get("$ref").and_then(Value::as_str) {
            Some(r) => {
                let mut cur = &self.doc;
                for part in r.trim_start_matches("#/").split('/') {
                    cur = &cur[part];
                }
                cur
            }
            None => v,
        }
    }

    /// Check one response against the contract. `template` is the path as written
    /// in the contract, for example `/api/devices/{device_id}`.
    pub fn check(
        &self,
        method: &str,
        template: &str,
        status: u16,
        body: Option<&Value>,
    ) -> Result<(), String> {
        let op = &self.doc["paths"][template][method.to_lowercase()];
        if op.is_null() {
            return Err(format!("{method} {template} is not in the contract"));
        }
        let responses = &op["responses"];
        let key = status.to_string();
        let resp = match responses.get(&key).or_else(|| responses.get("default")) {
            Some(r) => self.resolve(r),
            None => {
                return Err(format!(
                    "{method} {template}: status {status} is not documented"
                ))
            }
        };
        let json_schema = resp["content"]["application/json"].get("schema");
        match (json_schema, body) {
            (None, None) => Ok(()),
            (None, Some(b)) if resp.get("content").is_none() => Err(format!(
                "{method} {template} {status}: the contract documents no body, got {b}"
            )),
            (None, Some(_)) => Ok(()), // a non-JSON body (audio, event stream)
            (Some(_), None) => Err(format!(
                "{method} {template} {status}: the contract documents a JSON body, got none"
            )),
            (Some(schema), Some(body)) => {
                let mut schema = schema.clone();
                strict_schema(&mut schema, false);
                let mut root = schema;
                match root.as_object_mut() {
                    Some(m) => {
                        m.insert("components".into(), self.doc["components"].clone());
                    }
                    None => return Err("response schema is not an object".into()),
                }
                let validator = jsonschema::options()
                    .with_draft(Draft::Draft202012)
                    .should_validate_formats(true)
                    .build(&root)
                    .map_err(|e| format!("contract schema does not compile: {e}"))?;
                let errors: Vec<String> = validator
                    .iter_errors(body)
                    .map(|e| format!("  at {}: {}", e.instance_path, e))
                    .collect();
                if errors.is_empty() {
                    Ok(())
                } else {
                    Err(format!("{method} {template} {status} does not match the contract:\n{}\nbody: {body}", errors.join("\n")))
                }
            }
        }
    }
}

/// Forbid undeclared properties on every object schema that lists properties.
/// `unevaluatedProperties` (not `additionalProperties`) so `allOf` still works.
fn strict_schema(v: &mut Value, in_all_of: bool) {
    let Some(m) = v.as_object_mut() else { return };
    if m.contains_key("properties")
        && !in_all_of
        && !m.contains_key("additionalProperties")
        && !m.contains_key("unevaluatedProperties")
    {
        m.insert("unevaluatedProperties".into(), Value::Bool(false));
    }
    let keys: Vec<String> = m.keys().cloned().collect();
    for k in keys {
        let child = m.get_mut(&k).expect("key exists");
        match k.as_str() {
            "properties" | "patternProperties" | "$defs" => {
                if let Some(map) = child.as_object_mut() {
                    for (_, s) in map.iter_mut() {
                        strict_schema(s, false);
                    }
                }
            }
            "allOf" => {
                if let Some(arr) = child.as_array_mut() {
                    for s in arr {
                        strict_schema(s, true);
                    }
                }
            }
            "anyOf" | "oneOf" | "prefixItems" => {
                if let Some(arr) = child.as_array_mut() {
                    for s in arr {
                        strict_schema(s, false);
                    }
                }
            }
            "items" | "additionalProperties" | "not" | "if" | "then" | "else" | "contains" => {
                strict_schema(child, false)
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------ test server

pub struct TestServer {
    /// Listener header sent on every call once set with `act_as`.
    pub as_listener: std::sync::Mutex<Option<String>>,
    pub base: String,
    pub client: reqwest::Client,
    pub running: Option<Running>,
    pub contract: Arc<Contract>,
    pub clock: Arc<FakeClock>,
    pub dir: TempDir,
}

pub const DEVICE: &str = "test-device-0001";

impl TestServer {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Self::start_in(dir).await
    }

    pub async fn start_in(dir: TempDir) -> Self {
        Self::start_with(dir, |_| {}).await
    }

    pub async fn start_with(dir: TempDir, tweak: impl FnOnce(&mut Config)) -> Self {
        let clock = Arc::new(FakeClock::new(
            "2026-01-15T12:00:00Z".parse().expect("time"),
        ));
        let mut config = Config::for_data_dir(dir.path());
        tweak(&mut config);
        let as_clock: Arc<dyn Clock> = clock.clone();
        let running = spawn(config, as_clock).await.expect("server starts");
        TestServer {
            as_listener: std::sync::Mutex::new(None),
            base: format!("http://{}", running.addr),
            client: reqwest::Client::new(),
            running: Some(running),
            contract: Arc::new(Contract::load()),
            clock,
            dir,
        }
    }

    /// Send a request, assert the status, check the response against the contract,
    /// and return the JSON body (Null for no body).
    pub async fn call(
        &self,
        method: Method,
        template: &str,
        path: &str,
        device: Option<&str>,
        body: Option<Value>,
        expect: u16,
    ) -> Value {
        let mut req = self
            .client
            .request(method.clone(), format!("{}{}", self.base, path));
        if let Some(d) = device {
            req = req.header("x-bardic-device", d);
        }
        if let Some(l) = self.as_listener.lock().unwrap().as_ref() {
            req = req.header("x-bardic-listener", l.as_str());
        }
        if let Some(b) = &body {
            req = req.json(b);
        }
        let resp = req.send().await.expect("request sends");
        let status = resp.status().as_u16();
        let text = resp.text().await.expect("body");
        let json: Value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text.clone()))
        };
        assert_eq!(
            status, expect,
            "{method} {path}: expected {expect}, got {status}: {text}"
        );
        let body_opt = if json.is_null() { None } else { Some(&json) };
        if let Err(e) = self
            .contract
            .check(method.as_str(), template, status, body_opt)
        {
            panic!("{e}");
        }
        json
    }

    pub async fn get(&self, template: &str, path: &str, expect: u16) -> Value {
        self.call(Method::GET, template, path, Some(DEVICE), None, expect)
            .await
    }

    pub async fn patch(&self, template: &str, path: &str, body: Value, expect: u16) -> Value {
        self.call(
            Method::PATCH,
            template,
            path,
            Some(DEVICE),
            Some(body),
            expect,
        )
        .await
    }

    pub async fn post(&self, template: &str, path: &str, body: Value, expect: u16) -> Value {
        self.call(
            Method::POST,
            template,
            path,
            Some(DEVICE),
            Some(body),
            expect,
        )
        .await
    }

    pub async fn put(&self, template: &str, path: &str, body: Value, expect: u16) -> Value {
        self.call(
            Method::PUT,
            template,
            path,
            Some(DEVICE),
            Some(body),
            expect,
        )
        .await
    }

    pub async fn delete(&self, template: &str, path: &str, expect: u16) -> Value {
        self.call(Method::DELETE, template, path, Some(DEVICE), None, expect)
            .await
    }

    /// Send the listener header on every following call.
    pub fn act_as(&self, listener_id: &str) {
        *self.as_listener.lock().unwrap() = Some(listener_id.to_string());
    }

    /// Stop sending the listener header.
    pub fn act_as_nobody(&self) {
        *self.as_listener.lock().unwrap() = None;
    }

    /// Upload a book file; returns (status, body) after checking the response against the contract.
    pub async fn upload(&self, file_name: &str, bytes: Vec<u8>, key: Option<&str>) -> (u16, Value) {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name.to_string());
        let form = reqwest::multipart::Form::new().part("file", part);
        let mut req = self
            .client
            .post(format!("{}/api/imports", self.base))
            .header("x-bardic-device", DEVICE)
            .multipart(form);
        if let Some(k) = key {
            req = req.header("idempotency-key", k);
        }
        let resp = req.send().await.expect("upload sends");
        let status = resp.status().as_u16();
        let json: Value = resp.json().await.expect("json body");
        if let Err(e) = self
            .contract
            .check("POST", "/api/imports", status, Some(&json))
        {
            panic!("{e}");
        }
        (status, json)
    }

    /// Poll an import until it stops changing; every poll is checked against the contract.
    pub async fn wait_import(&self, id: &str) -> Value {
        for _ in 0..200 {
            let b = self
                .get(
                    "/api/imports/{import_id}",
                    &format!("/api/imports/{id}"),
                    200,
                )
                .await;
            if matches!(b["state"].as_str(), Some("done" | "failed" | "cancelled")) {
                return b;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("import {id} did not finish");
    }

    /// Import a file and return the finished book id.
    pub async fn add_book(&self, file_name: &str, bytes: Vec<u8>) -> String {
        let (st, imp) = self.upload(file_name, bytes, None).await;
        assert_eq!(st, 202);
        let done = self.wait_import(imp["id"].as_str().unwrap()).await;
        assert_eq!(done["state"], "done", "{done}");
        done["book_id"].as_str().unwrap().to_string()
    }

    /// A request whose response may not be JSON (audio). Checks the status against the
    /// contract and returns the headers and the raw body.
    pub async fn raw(
        &self,
        template: &str,
        path: &str,
        headers: &[(&str, &str)],
        expect: u16,
    ) -> (reqwest::header::HeaderMap, Vec<u8>) {
        let mut req = self.client.get(format!("{}{}", self.base, path));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.expect("request sends");
        let status = resp.status().as_u16();
        let hdrs = resp.headers().clone();
        let bytes = resp.bytes().await.expect("body").to_vec();
        assert_eq!(
            status, expect,
            "GET {path}: expected {expect}, got {status}"
        );
        let json: Option<Value> = if hdrs
            .get("content-type")
            .is_some_and(|c| c.to_str().unwrap_or("").starts_with("application/json"))
        {
            serde_json::from_slice(&bytes).ok()
        } else {
            None
        };
        if let Err(e) = self.contract.check("GET", template, status, json.as_ref()) {
            panic!("{e}");
        }
        (hdrs, bytes)
    }

    /// Create a listener and return its id.
    pub async fn listener(&self, name: &str) -> String {
        let b = self
            .post("/api/listeners", "/api/listeners", name_body(name), 201)
            .await;
        b["id"].as_str().expect("id").to_string()
    }

    pub async fn stop(mut self) -> TempDir {
        if let Some(r) = self.running.take() {
            r.stop().await;
        }
        self.dir
    }
}

pub fn name_body(name: &str) -> Value {
    json!({ "name": name })
}
