use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};

pub const MAX_BREEZE_CONCURRENCY: usize = 16;

fn parse_breeze_concurrency(raw: &str) -> Result<usize, String> {
    raw.parse::<usize>()
        .ok()
        .filter(|value| (1..=MAX_BREEZE_CONCURRENCY).contains(value))
        .ok_or_else(|| "Breeze concurrency must be an integer from 1 to 16.".to_string())
}

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

    /// Host names this server may be reached by, beyond IP addresses, `localhost`, single-word
    /// names and `.local`, `.lan`, `.home.arpa` and `.ts.net` names, which are always accepted.
    /// A browser page on a rebound public name (DNS rebinding) is refused with 403 `host_not_allowed`.
    #[arg(long = "allow-host", env = "BARDIC_ALLOW_HOSTS", value_delimiter = ',')]
    pub allow_hosts: Vec<String>,

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

    /// Most simultaneous Breeze speech requests, including free voice samples. Increase only when the source has capacity.
    #[arg(
        long,
        env = "BARDIC_BREEZE_CONCURRENCY",
        default_value_t = 1,
        value_parser = parse_breeze_concurrency
    )]
    pub breeze_concurrency: usize,

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
            allow_hosts: Vec::new(),
            server_name: Some("Test Bardic".to_string()),
            audio_chunk_chars: 2500,
            breeze_concurrency: 1,
            job_retry_ms: 2000,
            ffmpeg: "ffmpeg".to_string(),
            gemini_url: "http://127.0.0.1:1".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    fn parse(args: &[&str]) -> Result<Config, clap::Error> {
        // The default and flag tests must not depend on an operator's environment.
        let matches = Config::command()
            .mut_arg("breeze_concurrency", |arg| arg.env(None::<&str>))
            .try_get_matches_from(args)?;
        Config::from_arg_matches(&matches)
    }

    #[test]
    fn breeze_concurrency_defaults_to_one_and_accepts_bounded_flags() {
        assert_eq!(parse(&["bardic-server"]).unwrap().breeze_concurrency, 1);
        assert_eq!(Config::for_data_dir("synthetic").breeze_concurrency, 1);
        for value in ["1", "2", "16"] {
            assert_eq!(
                parse(&["bardic-server", "--breeze-concurrency", value])
                    .unwrap()
                    .breeze_concurrency,
                value.parse::<usize>().unwrap()
            );
        }
    }

    #[test]
    fn breeze_concurrency_rejects_zero_excessive_and_non_integer_flags() {
        for value in ["0", "17", "-1", "1.5", "many"] {
            assert!(parse(&["bardic-server", "--breeze-concurrency", value]).is_err());
        }
        let command = Config::command();
        let option = command
            .get_arguments()
            .find(|arg| arg.get_id() == "breeze_concurrency")
            .unwrap();
        assert_eq!(option.get_env().unwrap(), "BARDIC_BREEZE_CONCURRENCY");
    }
}
