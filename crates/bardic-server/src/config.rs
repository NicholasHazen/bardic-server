use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};

/// Server configuration. Flags win over environment variables.
#[derive(Debug, Clone, Parser)]
#[command(name = "bardic-server", version, about = "Bardic server")]
pub struct Config {
    /// Folder for the database, audio and lock file.
    #[arg(long, env = "BARDIC_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    /// Address to listen on. Loopback by default; binding to the network is an explicit choice.
    #[arg(long, env = "BARDIC_BIND", default_value = "127.0.0.1:8765")]
    pub bind: SocketAddr,

    /// Largest accepted book upload, in bytes.
    #[arg(long, env = "BARDIC_MAX_UPLOAD_BYTES", default_value_t = 31_457_280)]
    pub max_upload_bytes: u64,

    /// Browser origins allowed to call the API from another address, e.g. http://localhost:5173.
    /// Repeat the flag or separate with commas. Writes from any other origin are refused.
    #[arg(
        long = "allow-origin",
        env = "BARDIC_ALLOW_ORIGINS",
        value_delimiter = ','
    )]
    pub allow_origins: Vec<String>,

    /// Name shown on every device. Only used the first time; later changes go through the API.
    #[arg(long, env = "BARDIC_SERVER_NAME")]
    pub server_name: Option<String>,

    /// Most characters of chapter text sent to a voice server in one request.
    #[arg(
        long,
        env = "BARDIC_AUDIO_CHUNK_CHARS",
        default_value_t = 2500,
        hide = true
    )]
    pub audio_chunk_chars: usize,

    /// Where Gemini is reached. Only tests and proxies change this.
    #[arg(
        long,
        env = "BARDIC_GEMINI_URL",
        default_value = "https://generativelanguage.googleapis.com",
        hide = true
    )]
    pub gemini_url: String,

    /// The ffmpeg program used to encode exports.
    #[arg(long, env = "BARDIC_FFMPEG", default_value = "ffmpeg", hide = true)]
    pub ffmpeg: String,

    /// First wait, in milliseconds, before retrying an unreachable voice server (doubles each try).
    #[arg(long, env = "BARDIC_JOB_RETRY_MS", default_value_t = 2000, hide = true)]
    pub job_retry_ms: u64,
}

impl Config {
    pub fn for_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Config {
            data_dir: data_dir.into(),
            bind: "127.0.0.1:0".parse().expect("valid address"),
            max_upload_bytes: 31_457_280,
            allow_origins: Vec::new(),
            server_name: Some("Test Bardic".to_string()),
            audio_chunk_chars: 2500,
            job_retry_ms: 2000,
            ffmpeg: "ffmpeg".to_string(),
            gemini_url: "http://127.0.0.1:1".to_string(),
        }
    }
}
