//! Gemini speech: premium voices through the owner's API key.
//!
//! Gemini has no voice-list call: its prebuilt voices are a fixed set, offered
//! here with Google's own one-word descriptions. Checking a key reads the model
//! list, which is free and never speaks.

use super::{Catalog, CatalogVoice, Check};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::time::Duration;

pub const MODEL: &str = "gemini-3.8-flash-tts";

const VOICES: [(&str, &str); 30] = [
    ("Zephyr", "Bright"),
    ("Puck", "Upbeat"),
    ("Charon", "Informative"),
    ("Kore", "Firm"),
    ("Fenrir", "Excitable"),
    ("Leda", "Youthful"),
    ("Orus", "Firm"),
    ("Aoede", "Breezy"),
    ("Callirrhoe", "Easy-going"),
    ("Autonoe", "Bright"),
    ("Enceladus", "Breathy"),
    ("Iapetus", "Clear"),
    ("Umbriel", "Easy-going"),
    ("Algieba", "Smooth"),
    ("Despina", "Smooth"),
    ("Erinome", "Clear"),
    ("Algenib", "Gravelly"),
    ("Rasalgethi", "Informative"),
    ("Laomedeia", "Upbeat"),
    ("Achernar", "Soft"),
    ("Alnilam", "Firm"),
    ("Schedar", "Even"),
    ("Gacrux", "Mature"),
    ("Pulcherrima", "Forward"),
    ("Achird", "Friendly"),
    ("Zubenelgenubi", "Casual"),
    ("Vindemiatrix", "Gentle"),
    ("Sadachbia", "Lively"),
    ("Sadaltager", "Knowledgeable"),
    ("Sulafat", "Warm"),
];

/// The revision changes only when Bardic changes the model it speaks with.
pub fn revision() -> String {
    format!("{MODEL}/1")
}

pub fn voices() -> Vec<CatalogVoice> {
    VOICES
        .iter()
        .map(|(name, style)| CatalogVoice {
            external_id: name.to_string(),
            name: name.to_string(),
            description: style.to_string(),
            language: "und".into(),
            revision: revision(),
        })
        .collect()
}

pub fn client() -> Client {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client")
}

/// Check the key against the model list (free) and report the voices.
pub async fn check(base_url: &str, key: &str) -> Catalog {
    let resp = client()
        .get(format!("{base_url}/v1beta/models"))
        .header("x-goog-api-key", key)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(_) => {
            return Catalog::failed(
                Check::Unreachable,
                "Could not reach Gemini. Check the network connection.",
            )
        }
    };
    match resp.status() {
        StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Catalog::failed(Check::KeyRejected, "Gemini rejected the API key.")
        }
        s if !s.is_success() => {
            return Catalog::failed(
                Check::Unreachable,
                format!("Gemini answered HTTP {} when checking the key.", s.as_u16()),
            )
        }
        _ => {}
    }
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let has_model = body["models"].as_array().is_none_or(|m| {
        m.iter()
            .any(|x| x["name"].as_str().is_some_and(|n| n.ends_with(MODEL)))
    });
    let mut detail = format!("Connected. {} voices.", VOICES.len());
    if !has_model {
        detail.push_str(&format!(
            " This key's project does not list {MODEL}; speech may be refused."
        ));
    }
    Catalog {
        check: Check::Connected,
        detail,
        voices: voices(),
    }
}
