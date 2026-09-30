//! Breeze: the owner's own voice server on the local network.
//!
//! Only cloned voices are offered: a designed voice sounds different on every
//! request, so audio made with it could never be made the same again. A voice's
//! revision hashes only what changes speech (settings, instruction, reference
//! text and the reference clip), not its label or description.

use super::{Catalog, CatalogVoice, Check};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

const MAX_VOICES: usize = 200;

/// A plain http(s) server root: no credentials, path, query or fragment.
pub fn normalize_base_url(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    if s.is_empty() || s.len() > 500 {
        return Err("Enter the Breeze server address, for example http://host.local:7860.".into());
    }
    let url = reqwest::Url::parse(s).map_err(|_| {
        "Use an http:// or https:// address, for example http://host.local:7860.".to_string()
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(
            "Use an http:// or https:// address, for example http://host.local:7860.".into(),
        );
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(
            "Enter only the server address and port, without a path, query or credentials.".into(),
        );
    }
    Ok(s.to_string())
}

fn client() -> Client {
    // A LAN server: never through a system proxy, never follow redirects.
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .expect("http client")
}

fn revision(voice: &Value, reference_sha256: &str) -> String {
    let reference = voice.get("reference").cloned().unwrap_or(Value::Null);
    let material = json!({
        "schema": 1,
        "id": voice.get("id"),
        "created_at": voice.get("created_at"),
        "instruction": voice.get("instruction"),
        "settings": voice.get("settings"),
        "reference_text": reference.get("text"),
        "reference_sha256": reference_sha256,
    });
    // serde_json objects are sorted, so this text is canonical.
    hex(Sha256::digest(material.to_string().as_bytes()).as_slice())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn valid_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

fn clip(v: Option<&Value>, max: usize) -> String {
    v.and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(max)
        .collect()
}

/// Check the server and read its voices. Free: never generates audio.
pub async fn fetch_catalog(base_url: &str, api_key: Option<&str>) -> Catalog {
    let http = client();
    let auth = |r: reqwest::RequestBuilder| match api_key.filter(|k| !k.is_empty()) {
        Some(k) => r.bearer_auth(k),
        None => r,
    };
    let unreachable = || {
        Catalog::failed(
            Check::Unreachable,
            format!("Could not reach the Breeze server at {base_url}."),
        )
    };

    match http.get(format!("{base_url}/health")).send().await {
        Err(_) => return unreachable(),
        Ok(r) if r.status() == StatusCode::SERVICE_UNAVAILABLE => {
            return Catalog::failed(
                Check::Unreachable,
                "The Breeze server is starting or its model is loading. Try again in a minute.",
            )
        }
        Ok(r) if !r.status().is_success() => {
            return Catalog::failed(
                Check::Unreachable,
                format!(
                    "The Breeze server answered its health check with HTTP {}.",
                    r.status().as_u16()
                ),
            )
        }
        Ok(_) => {}
    }
    let listing = match auth(http.get(format!("{base_url}/v1/voices"))).send().await {
        Err(_) => return unreachable(),
        Ok(r) => r,
    };
    if listing.status() == StatusCode::UNAUTHORIZED || listing.status() == StatusCode::FORBIDDEN {
        return Catalog::failed(
            Check::KeyRejected,
            "The Breeze server rejected the API key.",
        );
    }
    if !listing.status().is_success() {
        return Catalog::failed(
            Check::Unreachable,
            format!(
                "The Breeze server answered HTTP {} when listing voices.",
                listing.status().as_u16()
            ),
        );
    }
    let Ok(body) = listing.json::<Value>().await else {
        return Catalog::failed(
            Check::Unreachable,
            "The Breeze server sent a voice list this server cannot read.",
        );
    };
    let mut voices = vec![];
    let mut hidden = 0usize;
    for v in body
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_VOICES)
    {
        let Some(id) = v.get("id").and_then(Value::as_str).filter(|i| valid_id(i)) else {
            continue;
        };
        if v.get("kind").and_then(Value::as_str) != Some("cloned") {
            hidden += 1;
            continue;
        }
        let sha = match auth(http.get(format!("{base_url}/v1/voices/{id}/reference")))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(b) => hex(Sha256::digest(&b).as_slice()),
                Err(_) => return unreachable(),
            },
            Ok(_) => {
                hidden += 1;
                continue;
            }
            Err(_) => return unreachable(),
        };
        let language = v
            .get("labels")
            .and_then(|l| l.get("language"))
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty() && l.len() <= 20)
            .unwrap_or("und")
            .to_string();
        let name = clip(v.get("name"), 200);
        voices.push(CatalogVoice {
            external_id: id.to_string(),
            name: if name.is_empty() {
                id.to_string()
            } else {
                name
            },
            description: clip(v.get("description"), 500),
            language,
            revision: revision(v, &sha),
        });
    }
    let mut detail = format!(
        "Connected. {} voice{}.",
        voices.len(),
        if voices.len() == 1 { "" } else { "s" }
    );
    if hidden > 0 {
        detail.push_str(&format!(" {hidden} designed or unreadable voice{} not offered: save a preview as a cloned voice in Breeze to use it.", if hidden == 1 { " is" } else { "s are" }));
    }
    Catalog {
        check: Check::Connected,
        detail,
        voices,
    }
}
