//! Voice sources: adapters that list the voices a provider offers.
//!
//! An adapter never speaks from here; listing is free and read-only. Speaking
//! arrives with audio generation. `local` has no engine in this server, and
//! `gemini` arrives with premium audio.

pub mod breeze;

/// What a check of a source found.
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Connected,
    Unreachable,
    KeyRejected,
}

impl Check {
    pub fn state(&self) -> &'static str {
        match self {
            Check::Connected => "connected",
            Check::Unreachable => "unreachable",
            Check::KeyRejected => "key_rejected",
        }
    }
}

/// A voice as a source reports it.
#[derive(Debug, Clone)]
pub struct CatalogVoice {
    pub external_id: String,
    pub name: String,
    pub description: String,
    pub language: String,
    /// Changes when the source changes how the voice sounds.
    pub revision: String,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub check: Check,
    /// Safe to show: never contains a key.
    pub detail: String,
    pub voices: Vec<CatalogVoice>,
}

impl Catalog {
    pub fn failed(check: Check, detail: impl Into<String>) -> Self {
        Catalog {
            check,
            detail: detail.into(),
            voices: vec![],
        }
    }
}
