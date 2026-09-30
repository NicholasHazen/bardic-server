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

    /// Name shown on every device. Only used the first time; later changes go through the API.
    #[arg(long, env = "BARDIC_SERVER_NAME")]
    pub server_name: Option<String>,
}

impl Config {
    pub fn for_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Config {
            data_dir: data_dir.into(),
            bind: "127.0.0.1:0".parse().expect("valid address"),
            max_upload_bytes: 31_457_280,
            server_name: Some("Test Bardic".to_string()),
        }
    }
}
