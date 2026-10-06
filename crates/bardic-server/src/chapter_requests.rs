//! Durable, indexed free requests. A chapter's ordered prefix is still owned by
//! jobs; these immutable files keep later requests until the prefix can use them.

use crate::{app::AppState, error::ApiError};
use rusqlite::params;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_PCM_BYTES: usize = 200 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct RetainedRequest {
    pub index: usize,
    pub pcm_bytes: usize,
    pub timings: Vec<Value>,
    pub sha256: String,
}

struct Row {
    index: i64,
    audio_id: String,
    pcm_bytes: i64,
    timings: String,
    sha256: String,
}

/// IDs in filenames are server-generated. Reject corrupt stored components
/// rather than allowing a recovery cleanup to escape its audiobook directory.
fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn relative_path(audiobook: &str, audio_id: &str, index: usize) -> String {
    format!("audio/{audiobook}/{audio_id}-{index}.request")
}

fn path(
    state: &AppState,
    audiobook: &str,
    audio_id: &str,
    index: usize,
) -> Result<PathBuf, ApiError> {
    if !safe_component(audiobook) || !safe_component(audio_id) {
        return Err(ApiError::internal("invalid request recovery filename"));
    }
    Ok(state
        .store
        .data_dir()
        .join(relative_path(audiobook, audio_id, index)))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_timings(timings: &[Value], pcm_bytes: usize) -> bool {
    let duration = crate::audio::pcm_ms(pcm_bytes);
    timings.iter().all(|t| {
        let Some((start, end)) = t["start_ms"].as_i64().zip(t["end_ms"].as_i64()) else {
            return false;
        };
        t["line_id"].as_str().is_some() && start >= 0 && end >= start && end <= duration
    })
}

async fn file_digest(path: &Path) -> Result<Option<String>, ApiError> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ApiError::internal(e)),
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.map_err(ApiError::internal)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Some(format!("{:x}", hasher.finalize())))
}

async fn sync_directories(state: &AppState, parent: &Path) -> Result<(), ApiError> {
    #[cfg(unix)]
    {
        let dirs = parent
            .ancestors()
            .take_while(|p| p.starts_with(state.store.data_dir()))
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            for dir in dirs {
                std::fs::File::open(dir)?.sync_all()?;
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    }
    #[cfg(not(unix))]
    let _ = (state, parent);
    Ok(())
}

/// The caller owns this index and has already durably initialized chapter_parts.
/// No database row or progress can refer to bytes before their durable rename.
pub(crate) async fn persist(
    state: &AppState,
    audiobook: &str,
    chapter: &str,
    audio_id: &str,
    index: usize,
    pcm: &[u8],
    timings: &[Value],
) -> Result<RetainedRequest, ApiError> {
    if pcm.is_empty()
        || !pcm.len().is_multiple_of(2)
        || pcm.len() > MAX_PCM_BYTES
        || !valid_timings(timings, pcm.len())
    {
        return Err(ApiError::internal("invalid retained request audio size"));
    }
    let index_sql = i64::try_from(index).map_err(ApiError::internal)?;
    let (a, c) = (audiobook.to_string(), chapter.to_string());
    let already_retained = state.store.run(move |conn| {
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM chapter_requests WHERE audiobook_id=?1 AND chapter_id=?2 AND request_index=?3)", params![a, c, index_sql], |r| r.get::<_, bool>(0))?)
    }).await?;
    if already_retained {
        return Err(ApiError::internal("retained request index already exists"));
    }
    let full = path(state, audiobook, audio_id, index)?;
    let dir = full
        .parent()
        .ok_or_else(|| ApiError::internal("request recovery path has no parent"))?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(ApiError::internal)?;
    let temp = full.with_extension(format!("{}.part", state.new_id()));
    let sha256 = digest(pcm);
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
            .map_err(ApiError::internal)?;
        file.write_all(pcm).await.map_err(ApiError::internal)?;
        file.sync_all().await.map_err(ApiError::internal)?;
        drop(file);
        tokio::fs::rename(&temp, &full)
            .await
            .map_err(ApiError::internal)?;
        sync_directories(state, dir).await?;
        let (a, c, id, bytes, ts, sha) = (
            audiobook.to_string(),
            chapter.to_string(),
            audio_id.to_string(),
            pcm.len() as i64,
            serde_json::to_string(timings).map_err(ApiError::internal)?,
            sha256.clone(),
        );
        state
            .store
            .run(move |conn| {
                // A deletion or explicit space release may have removed the parent
                // while the free provider request was finishing. Never recreate it.
                let n = conn.execute(
                    "INSERT INTO chapter_requests(audiobook_id,chapter_id,request_index,audio_id,pcm_bytes,timings,sha256)
                     SELECT ?1,?2,?3,?4,?5,?6,?7 FROM chapter_parts
                     WHERE audiobook_id=?1 AND chapter_id=?2 AND audio_id=?4",
                    params![a, c, index_sql, id, bytes, ts, sha],
                )?;
                if n != 1 {
                    return Err(ApiError::internal("request recovery parent was removed"));
                }
                Ok(())
            })
            .await?;
        Ok(RetainedRequest {
            index,
            pcm_bytes: pcm.len(),
            timings: timings.to_vec(),
            sha256,
        })
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
        // No row was committed on an error. A failed rename/fsync/insert can
        // leave an unreferenced final file, which must not become progress.
        let _ = tokio::fs::remove_file(&full).await;
    }
    result
}

/// Load once before launching this chapter's writers. Invalid/mismatched rows,
/// and rows already absorbed by a committed prefix, are no longer reusable.
pub(crate) async fn load(
    state: &AppState,
    audiobook: &str,
    chapter: &str,
    audio_id: &str,
    chunk_count: usize,
    prefix_done: usize,
) -> Result<Vec<RetainedRequest>, ApiError> {
    let (a, c) = (audiobook.to_string(), chapter.to_string());
    let rows: Vec<Row> = state
        .store
        .run(move |conn| {
            Ok(conn
                .prepare("SELECT request_index,audio_id,pcm_bytes,timings,sha256 FROM chapter_requests WHERE audiobook_id=?1 AND chapter_id=?2 ORDER BY request_index")?
                .query_map(params![a, c], |r| {
                    Ok(Row {
                        index: r.get(0)?,
                        audio_id: r.get(1)?,
                        pcm_bytes: r.get(2)?,
                        timings: r.get(3)?,
                        sha256: r.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    let mut valid = Vec::new();
    for row in rows {
        let timings = serde_json::from_str::<Vec<Value>>(&row.timings).ok();
        let candidate = usize::try_from(row.index).ok().filter(|i| {
            *i >= prefix_done
                && *i < chunk_count
                && row.audio_id == audio_id
                && row.pcm_bytes > 0
                && row.pcm_bytes <= MAX_PCM_BYTES as i64
                && row.pcm_bytes % 2 == 0
                && timings
                    .as_ref()
                    .is_some_and(|ts| valid_timings(ts, row.pcm_bytes as usize))
                && row.sha256.len() == 64
                && row.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        });
        let mut verified = false;
        if let Some(index) = candidate {
            let full = path(state, audiobook, audio_id, index)?;
            match tokio::fs::metadata(&full).await {
                Ok(m) if m.is_file() && m.len() == row.pcm_bytes as u64 => {
                    verified = file_digest(&full).await?.as_deref() == Some(&row.sha256);
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ApiError::internal(e)),
            }
            if verified {
                valid.push(RetainedRequest {
                    index,
                    pcm_bytes: row.pcm_bytes as usize,
                    timings: timings.expect("candidate parsed timings"),
                    sha256: row.sha256.clone(),
                });
            }
        }
        if !verified {
            let (a, c, id, index) = (
                audiobook.to_string(),
                chapter.to_string(),
                row.audio_id.clone(),
                row.index,
            );
            state.store.run(move |conn| {
                conn.execute("DELETE FROM chapter_requests WHERE audiobook_id=?1 AND chapter_id=?2 AND request_index=?3 AND audio_id=?4", params![a, c, index, id])?;
                Ok(())
            }).await?;
            if safe_component(audiobook) && safe_component(&row.audio_id) {
                if let Ok(index) = usize::try_from(row.index) {
                    let _ =
                        tokio::fs::remove_file(path(state, audiobook, &row.audio_id, index)?).await;
                }
            }
        }
    }
    Ok(valid)
}

/// Verify again when assembling, so a changed/truncated cache cannot become Ready.
pub(crate) async fn read(
    state: &AppState,
    audiobook: &str,
    audio_id: &str,
    request: &RetainedRequest,
) -> Result<Vec<u8>, ApiError> {
    let full = path(state, audiobook, audio_id, request.index)?;
    let metadata = tokio::fs::metadata(&full)
        .await
        .map_err(ApiError::internal)?;
    if !metadata.is_file()
        || metadata.len() != request.pcm_bytes as u64
        || request.pcm_bytes > MAX_PCM_BYTES
    {
        return Err(ApiError::internal("retained request size changed"));
    }
    let pcm = tokio::fs::read(full).await.map_err(ApiError::internal)?;
    if pcm.len() != request.pcm_bytes || digest(&pcm) != request.sha256 {
        return Err(ApiError::internal("retained request audio changed"));
    }
    Ok(pcm)
}

/// Call only after the ordered prefix transaction has committed these indices.
/// Removing the index rows first makes a crash leave harmless extra files.
pub(crate) async fn discard_prefix(
    state: &AppState,
    audiobook: &str,
    chapter: &str,
    audio_id: &str,
    prefix_done: usize,
) -> Result<(), ApiError> {
    let done = i64::try_from(prefix_done).map_err(ApiError::internal)?;
    let (a, c, id) = (
        audiobook.to_string(),
        chapter.to_string(),
        audio_id.to_string(),
    );
    let indices = state.store.run(move |conn| {
        let tx = conn.transaction()?;
        let indices = tx.prepare("SELECT request_index FROM chapter_requests WHERE audiobook_id=?1 AND chapter_id=?2 AND audio_id=?3 AND request_index<?4")?
            .query_map(params![a, c, id, done], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        tx.execute("DELETE FROM chapter_requests WHERE audiobook_id=?1 AND chapter_id=?2 AND audio_id=?3 AND request_index<?4", params![a, c, id, done])?;
        tx.commit()?;
        Ok(indices)
    }).await?;
    for index in indices {
        if let Ok(index) = usize::try_from(index) {
            let _ = tokio::fs::remove_file(path(state, audiobook, audio_id, index)?).await;
        }
    }
    Ok(())
}

/// The caller has finished all writers and committed Ready (or discarded the
/// parent). No scan may remove files belonging to an active chapter attempt.
pub(crate) async fn cleanup(
    state: &AppState,
    audiobook: &str,
    audio_id: &str,
) -> Result<(), ApiError> {
    let full = path(state, audiobook, audio_id, 0)?;
    let dir = full.parent().expect("request path has parent");
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ApiError::internal(e)),
    };
    let prefix = format!("{audio_id}-");
    while let Some(entry) = entries.next_entry().await.map_err(ApiError::internal)? {
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|s| s.strip_prefix(&prefix)) else {
            continue;
        };
        let index = suffix.strip_suffix(".request").or_else(|| {
            // Dropping a pending writer during its file await can leave its
            // private temporary file. All writers have ended before this scan.
            let (index, temporary_id) = suffix.strip_suffix(".part")?.split_once('.')?;
            safe_component(temporary_id).then_some(index)
        });
        if index.is_some_and(|index| index.parse::<usize>().is_ok())
            && entry
                .file_type()
                .await
                .map_err(ApiError::internal)?
                .is_file()
        {
            tokio::fs::remove_file(entry.path())
                .await
                .map_err(ApiError::internal)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clock::SystemClock, config::Config};
    use serde_json::json;
    use std::sync::Arc;

    fn fixture() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(Config::for_data_dir(dir.path()), Arc::new(SystemClock)).unwrap();
        state.store.run_blocking(|c| {
            c.execute_batch("INSERT INTO books(id,title,state,added_at) VALUES('book','Synthetic request cache','readable','2026-10-06T00:00:00Z');
                INSERT INTO chapters(id,book_id,idx,title,kind,text,text_sha256,word_count) VALUES('chapter','book',0,'One','story','A synthetic line.','text',3);
                INSERT INTO voices(id,source_id,external_id,name,tier,language,revision,updated_at) VALUES('voice','breeze','voice','Synthetic','free','en','r1','2026-10-06T00:00:00Z');
                INSERT INTO audiobooks(id,book_id,voice_id,voice_name,voice_revision,created_at) VALUES('audiobook','book','voice','Synthetic','r1','2026-10-06T00:00:00Z');
                INSERT INTO chapter_parts(audiobook_id,chapter_id,audio_id,chunk_chars,chunks_done,pcm_bytes,timings) VALUES('audiobook','chapter','audio',100,0,0,'[]');")
        }).unwrap();
        (dir, state)
    }

    fn timings() -> Vec<Value> {
        vec![json!({"line_id":"line", "start_ms":0, "end_ms":100})]
    }

    async fn retain(state: &AppState, index: usize) -> RetainedRequest {
        persist(
            state,
            "audiobook",
            "chapter",
            "audio",
            index,
            &vec![index as u8; 4800],
            &timings(),
        )
        .await
        .unwrap()
    }

    fn count(state: &AppState) -> i64 {
        state
            .store
            .run_blocking(|c| {
                c.query_row("SELECT COUNT(*) FROM chapter_requests", [], |r| r.get(0))
            })
            .unwrap()
    }

    #[tokio::test]
    async fn finished_requests_load_in_index_order_without_advancing_the_prefix() {
        let (_dir, state) = fixture();
        retain(&state, 2).await;
        retain(&state, 0).await;
        let requests = load(&state, "audiobook", "chapter", "audio", 3, 0)
            .await
            .unwrap();
        assert_eq!(requests.iter().map(|r| r.index).collect::<Vec<_>>(), [0, 2]);
        for request in &requests {
            assert_eq!(request.timings, timings());
            assert_eq!(
                read(&state, "audiobook", "audio", request).await.unwrap(),
                vec![request.index as u8; 4800]
            );
        }
        let done: i64 = state
            .store
            .run_blocking(|c| {
                c.query_row("SELECT chunks_done FROM chapter_parts", [], |r| r.get(0))
            })
            .unwrap();
        assert_eq!(done, 0);
        assert!(persist(
            &state,
            "audiobook",
            "chapter",
            "audio",
            2,
            &[0; 4800],
            &timings()
        )
        .await
        .is_err());
        assert_eq!(
            read(&state, "audiobook", "audio", &requests[1])
                .await
                .unwrap(),
            vec![2; 4800]
        );
    }

    #[tokio::test]
    async fn corrupt_sizes_hashes_and_timings_are_discarded_individually() {
        let (_dir, state) = fixture();
        for index in 0..4 {
            retain(&state, index).await;
        }
        tokio::fs::write(path(&state, "audiobook", "audio", 0).unwrap(), [0; 2])
            .await
            .unwrap();
        tokio::fs::write(path(&state, "audiobook", "audio", 1).unwrap(), [7; 4800])
            .await
            .unwrap();
        state
            .store
            .run_blocking(|c| {
                c.execute(
                    "UPDATE chapter_requests SET timings='[{}]' WHERE request_index=2",
                    [],
                )
            })
            .unwrap();
        let requests = load(&state, "audiobook", "chapter", "audio", 4, 0)
            .await
            .unwrap();
        assert_eq!(requests.iter().map(|r| r.index).collect::<Vec<_>>(), [3]);
        assert_eq!(count(&state), 1);
        for index in 0..3 {
            assert!(!path(&state, "audiobook", "audio", index).unwrap().exists());
        }
        tokio::fs::write(path(&state, "audiobook", "audio", 3).unwrap(), [9; 4800])
            .await
            .unwrap();
        assert!(read(&state, "audiobook", "audio", &requests[0])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn stale_identity_and_out_of_range_indices_do_not_escape_the_cache() {
        let (_dir, state) = fixture();
        retain(&state, 0).await;
        retain(&state, 1).await;
        retain(&state, 7).await;
        let old = path(&state, "audiobook", "old-audio", 1).unwrap();
        tokio::fs::rename(path(&state, "audiobook", "audio", 1).unwrap(), &old)
            .await
            .unwrap();
        state
            .store
            .run_blocking(|c| {
                c.execute(
                    "UPDATE chapter_requests SET audio_id='old-audio' WHERE request_index=1",
                    [],
                )
            })
            .unwrap();
        let requests = load(&state, "audiobook", "chapter", "audio", 3, 0)
            .await
            .unwrap();
        assert_eq!(requests.iter().map(|r| r.index).collect::<Vec<_>>(), [0]);
        assert_eq!(count(&state), 1);
        assert!(!old.exists());
        assert!(!path(&state, "audiobook", "audio", 7).unwrap().exists());
    }

    #[tokio::test]
    async fn committed_prefix_cleanup_and_restart_cleanup_leave_later_requests() {
        let (_dir, state) = fixture();
        for index in 0..3 {
            retain(&state, index).await;
        }
        state
            .store
            .run_blocking(|c| c.execute("UPDATE chapter_parts SET chunks_done=1", []))
            .unwrap();
        discard_prefix(&state, "audiobook", "chapter", "audio", 1)
            .await
            .unwrap();
        assert_eq!(count(&state), 2);
        assert!(!path(&state, "audiobook", "audio", 0).unwrap().exists());
        // A crash after the next prefix commit, before discard_prefix, is harmless.
        state
            .store
            .run_blocking(|c| c.execute("UPDATE chapter_parts SET chunks_done=2", []))
            .unwrap();
        let requests = load(&state, "audiobook", "chapter", "audio", 3, 2)
            .await
            .unwrap();
        assert_eq!(requests.iter().map(|r| r.index).collect::<Vec<_>>(), [2]);
        assert_eq!(count(&state), 1);
        assert!(!path(&state, "audiobook", "audio", 1).unwrap().exists());
    }

    #[tokio::test]
    async fn deleting_the_parent_cascades_and_final_cleanup_preserves_other_audio() {
        let (_dir, state) = fixture();
        retain(&state, 1).await;
        let other = path(&state, "audiobook", "other-audio", 1).unwrap();
        tokio::fs::write(&other, [1; 2]).await.unwrap();
        let ready = state.store.data_dir().join("audio/audiobook/audio.wav");
        tokio::fs::write(&ready, [1; 44]).await.unwrap();
        let unfinished = state
            .store
            .data_dir()
            .join("audio/audiobook/audio-3.synthetic.part");
        tokio::fs::write(&unfinished, [1; 2]).await.unwrap();
        state
            .store
            .run_blocking(|c| c.execute("DELETE FROM chapter_parts", []))
            .unwrap();
        assert_eq!(count(&state), 0);
        cleanup(&state, "audiobook", "audio").await.unwrap();
        assert!(!path(&state, "audiobook", "audio", 1).unwrap().exists());
        assert!(other.exists());
        assert!(ready.exists());
        assert!(!unfinished.exists());
        assert!(persist(
            &state,
            "audiobook",
            "chapter",
            "audio",
            2,
            &[2; 4800],
            &timings()
        )
        .await
        .is_err());
        assert!(!path(&state, "audiobook", "audio", 2).unwrap().exists());
        assert_eq!(count(&state), 0);
    }

    #[tokio::test]
    async fn explicit_space_release_removes_both_prefix_and_indexed_requests() {
        let (_dir, state) = fixture();
        retain(&state, 2).await;
        let prefix = state.store.data_dir().join("audio/audiobook/chapter.part");
        tokio::fs::write(&prefix, [0; 44]).await.unwrap();
        let device = crate::app::DeviceCtx {
            id: "device".into(),
            name: "Synthetic".into(),
        };
        let _ = crate::api::space::free_space(
            axum::extract::State(state.clone()),
            device,
            axum::extract::Path("audiobook".into()),
        )
        .await
        .unwrap();
        assert_eq!(count(&state), 0);
        assert!(!prefix.exists());
        assert!(!path(&state, "audiobook", "audio", 2).unwrap().exists());
    }

    #[tokio::test]
    async fn live_backup_keeps_ready_audio_and_omits_recovery_files_and_rows() {
        let (_dir, state) = fixture();
        retain(&state, 2).await;
        let prefix = state.store.data_dir().join("audio/audiobook/chapter.part");
        tokio::fs::write(&prefix, [0; 44]).await.unwrap();
        let ready = state.store.data_dir().join("audio/audiobook/ready.wav");
        tokio::fs::write(&ready, [0; 44]).await.unwrap();
        state.store.run_blocking(|c| c.execute("INSERT INTO audio(id,audiobook_id,chapter_id,voice_revision,path,bytes,sha256,duration_ms,content_type,timings,created_at) VALUES('ready','audiobook','chapter','r1','audio/audiobook/ready.wav',44,'synthetic',0,'audio/wav','[]','2026-10-06T00:00:00Z')", [])).unwrap();
        let device = crate::app::DeviceCtx {
            id: "device".into(),
            name: "Synthetic".into(),
        };
        let (_, axum::Json(backup)) =
            crate::api::admin::create_backup(axum::extract::State(state.clone()), device)
                .await
                .unwrap();
        let backup_id = backup["id"].as_str().unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let id = backup_id.to_string();
            let status: String = state
                .store
                .run(move |c| {
                    Ok(c.query_row("SELECT state FROM backups WHERE id=?1", [id], |r| r.get(0))?)
                })
                .await
                .unwrap();
            if status != "running" {
                assert_eq!(status, "done");
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "backup did not finish"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let backup_dir = state.store.data_dir().join("backups").join(backup_id);
        assert!(backup_dir.join("media/audio/audiobook/ready.wav").exists());
        assert!(!backup_dir
            .join("media/audio/audiobook/chapter.part")
            .exists());
        assert!(!backup_dir
            .join("media/audio/audiobook/audio-2.request")
            .exists());
        let copied = rusqlite::Connection::open_with_flags(
            backup_dir.join("bardic.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let counts: (i64, i64, i64) = copied.query_row("SELECT (SELECT COUNT(*) FROM audio), (SELECT COUNT(*) FROM chapter_parts), (SELECT COUNT(*) FROM chapter_requests)", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(counts, (1, 0, 0));
        assert_eq!(count(&state), 1, "the live request remains reusable");
        assert!(prefix.exists());
        assert!(path(&state, "audiobook", "audio", 2).unwrap().exists());
    }
}
