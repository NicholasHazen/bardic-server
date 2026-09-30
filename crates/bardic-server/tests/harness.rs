//! The conformance checker must itself catch what the rules say it must.
mod common;
use common::Contract;
use serde_json::json;

#[test]
fn accepts_a_valid_response() {
    let c = Contract::load();
    let body = json!({ "ok": true, "time": "2026-01-15T12:00:00.000Z" });
    c.check("GET", "/api/health", 200, Some(&body))
        .expect("valid");
}

#[test]
fn rejects_an_undeclared_field() {
    let c = Contract::load();
    let body = json!({ "ok": true, "time": "2026-01-15T12:00:00.000Z", "surprise": 1 });
    let err = c
        .check("GET", "/api/health", 200, Some(&body))
        .expect_err("extra field must fail");
    assert!(
        err.contains("surprise") || err.contains("Unevaluated"),
        "{err}"
    );
}

#[test]
fn rejects_a_missing_required_field() {
    let c = Contract::load();
    let body = json!({ "ok": true });
    assert!(c.check("GET", "/api/health", 200, Some(&body)).is_err());
}

#[test]
fn rejects_a_wrong_type() {
    let c = Contract::load();
    let body = json!({ "ok": "yes", "time": "2026-01-15T12:00:00.000Z" });
    assert!(c.check("GET", "/api/health", 200, Some(&body)).is_err());
}

#[test]
fn rejects_a_bad_date_time() {
    let c = Contract::load();
    let body = json!({ "ok": true, "time": "yesterday" });
    assert!(c.check("GET", "/api/health", 200, Some(&body)).is_err());
}

#[test]
fn rejects_an_undocumented_status() {
    let c = Contract::load();
    let body = json!({ "code": "x", "detail": "y" });
    let err = c
        .check("GET", "/api/health", 418, Some(&body))
        .expect_err("418 is not documented");
    assert!(err.contains("not documented"), "{err}");
}

#[test]
fn follows_refs_and_allof() {
    // Error body through a $ref to a shared response.
    let c = Contract::load();
    let ok = json!({ "code": "name_invalid", "detail": "bad" });
    c.check("PATCH", "/api/server", 400, Some(&ok))
        .expect("valid error");
    let bad = json!({ "code": "name_invalid", "detail": "bad", "extra": true });
    assert!(c.check("PATCH", "/api/server", 400, Some(&bad)).is_err());
}

#[test]
fn server_constant_matches_contract_version() {
    assert_eq!(bardic_server::API_VERSION, Contract::load().version());
}
