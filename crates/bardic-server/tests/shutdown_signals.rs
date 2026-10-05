//! Real process signals must preserve the sample drain and single-writer lock.
#![cfg(unix)]

mod common;

use common::{gemini::FakeGemini, Contract};
use reqwest::Client;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tokio::process::{Child, Command};

const WAIT: Duration = Duration::from_secs(10);

struct ServerProcess {
    child: Child,
    base: String,
    log: std::path::PathBuf,
}

fn command(data: &Path, gemini_url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bardic-server"));
    command
        .arg("--data-dir")
        .arg(data)
        .arg("--gemini-url")
        .arg(gemini_url)
        .env("RUST_LOG", "info")
        .env_remove("BARDIC_ALLOW_ORIGINS")
        .env_remove("BARDIC_ALLOW_HOSTS")
        .kill_on_drop(true);
    command
}

async fn start(data: &Path, gemini_url: &str, client: &Client) -> ServerProcess {
    // The listener is reserved only long enough to choose an unused loopback port.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let base = format!("http://127.0.0.1:{port}");
    let log = data.join(format!("process-{port}.log"));
    let stdout = std::fs::File::create(&log).unwrap();
    let child = command(data, gemini_url)
        .args(["--bind", &format!("127.0.0.1:{port}")])
        .stdout(stdout.try_clone().unwrap())
        .stderr(stdout)
        .spawn()
        .unwrap();
    let mut server = ServerProcess { child, base, log };
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                panic!(
                    "server exited before listening ({status}): {}",
                    std::fs::read_to_string(&server.log).unwrap()
                );
            }
            if let Ok(response) = client
                .get(format!("{}/api/health", server.base))
                .send()
                .await
            {
                if response.status().is_success() {
                    let body: Value = response.json().await.unwrap();
                    Contract::load()
                        .check("GET", "/api/health", 200, Some(&body))
                        .unwrap();
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child server starts");
    server
}

fn signal(child: &Child, name: &str) {
    let status = std::process::Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().expect("server is running").to_string())
        .status()
        .expect("send Unix signal");
    assert!(status.success());
}

fn ledger(data: &Path) -> (i64, i64, i64, i64) {
    let conn =
        Connection::open_with_flags(data.join("bardic.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    conn.query_row(
        "SELECT COUNT(*),COALESCE(SUM(status='reserved'),0),COALESCE(SUM(status='known'),0),COALESCE(SUM(status='unknown'),0) FROM spend",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

async fn exits_cleanly(server: &mut ServerProcess) {
    let status = tokio::time::timeout(WAIT, server.child.wait())
        .await
        .expect("graceful drain finishes")
        .unwrap();
    assert!(
        status.success(),
        "server should exit normally after its drain, got {status}: {}",
        std::fs::read_to_string(&server.log).unwrap()
    );
}

async fn sample_is_drained_on_signal(name: &str) {
    let data = tempfile::tempdir().unwrap();
    let provider = FakeGemini::start("shutdown-test-key").await;
    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let contract = Contract::load();
    let mut server = start(data.path(), &provider.url, &client).await;
    let response = client
        .put(format!("{}/api/voice-sources/gemini", server.base))
        .header("x-bardic-device", common::DEVICE)
        .json(&json!({"api_key": "shutdown-test-key"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    contract
        .check("PUT", "/api/voice-sources/{source_id}", 200, Some(&body))
        .unwrap();
    let response = client
        .get(format!("{}/api/voices?source_id=gemini", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let voices: Value = response.json().await.unwrap();
    contract
        .check("GET", "/api/voices", 200, Some(&voices))
        .unwrap();
    let voice = voices["items"][0]["id"].as_str().unwrap();
    let path = format!("/api/voices/{voice}/sample");
    provider.state.lock().unwrap().delay_ms = 2_000;
    let waiter = {
        let client = client.clone();
        let url = format!("{}{path}", server.base);
        tokio::spawn(async move {
            client
                .get(url)
                .header("x-bardic-device", common::DEVICE)
                .send()
                .await
        })
    };
    tokio::time::timeout(WAIT, async {
        while provider.received() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fake provider admits the sample");
    assert_eq!(ledger(data.path()), (1, 1, 0, 0));
    waiter.abort(); // The sample remains owned by the server after HTTP cancellation.
    signal(&server.child, name);
    tokio::time::timeout(WAIT, async {
        loop {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "signal must drain the admitted sample instead of terminating the process"
            );
            if std::fs::read_to_string(&server.log)
                .unwrap()
                .contains("shutting down")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("signal enters graceful shutdown");
    let contender = command(data.path(), &provider.url)
        .args(["--bind", "127.0.0.1:0"])
        .output()
        .await
        .unwrap();
    assert!(!contender.status.success());
    assert!(String::from_utf8_lossy(&contender.stderr).contains("already using"));
    exits_cleanly(&mut server).await;
    assert_eq!(ledger(data.path()), (1, 0, 1, 0));
    let conn = Connection::open_with_flags(
        data.path().join("bardic.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let (sample, bytes): (String, i64) = conn
        .query_row("SELECT path,bytes FROM voice_samples", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    drop(conn);
    assert_eq!(
        std::fs::metadata(data.path().join(sample)).unwrap().len(),
        bytes as u64
    );

    // Starting a second real process proves the lock was released only after the
    // cache and ledger were committed. A cached read must not call the provider.
    let mut restarted = start(data.path(), &provider.url, &client).await;
    let response = client
        .get(format!("{}{path}", restarted.base))
        .header("x-bardic-device", common::DEVICE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    contract
        .check("GET", "/api/voices/{voice_id}/sample", 200, None)
        .unwrap();
    assert_eq!(&response.bytes().await.unwrap()[..4], b"RIFF");
    assert_eq!(provider.received(), 1);
    assert_eq!(ledger(data.path()), (1, 0, 1, 0));
    signal(&restarted.child, name);
    exits_cleanly(&mut restarted).await;
    provider.stop();
}

#[tokio::test]
async fn sigterm_drains_cancelled_sample_before_exit_and_lock_release() {
    sample_is_drained_on_signal("TERM").await;
}

#[tokio::test]
async fn sigint_keeps_the_existing_graceful_drain() {
    sample_is_drained_on_signal("INT").await;
}
