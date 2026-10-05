//! M5: freeing space, offline manifests and sync checks, permanent deletion with undo, backups and export.
mod common;
use chrono::Duration;
use common::{breeze::FakeBreeze, gemini::FakeGemini, TestServer, DEVICE};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::Duration as StdDuration;

const SRC: &str = "/api/voice-sources/{source_id}";
const SPACE: &str = "/api/audiobooks/{audiobook_id}/space";
const DEL: &str = "/api/books/{book_id}/deletion";

struct Fx {
    s: TestServer,
    b: FakeBreeze,
    book: String,
    audiobook: String,
    chapters: Vec<String>,
}

fn quick(c: &mut bardic_server::config::Config) {
    c.audio_chunk_chars = 400;
    c.job_retry_ms = 20;
}

impl Fx {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(tweak: impl FnOnce(&mut bardic_server::config::Config)) -> Self {
        let s = TestServer::start_with(tempfile::tempdir().unwrap(), |c| {
            quick(c);
            tweak(c);
        })
        .await;
        Self::on(s, FakeBreeze::start().await).await
    }

    async fn on(s: TestServer, b: FakeBreeze) -> Self {
        let l = s.listener("Nick").await;
        s.act_as(&l);
        s.put(
            SRC,
            "/api/voice-sources/breeze",
            json!({ "base_url": b.url }),
            200,
        )
        .await;
        let book = s
            .post("/api/books/sample", "/api/books/sample", json!({}), 201)
            .await["id"]
            .as_str()
            .unwrap()
            .to_string();
        let voice = s.get("/api/voices", "/api/voices", 200).await["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "Mara")
            .unwrap()["id"]
            .clone();
        let ab = s
            .post(
                "/api/books/{book_id}/audiobooks",
                &format!("/api/books/{book}/audiobooks"),
                json!({ "voice_id": voice }),
                201,
            )
            .await;
        let chapters = s
            .get(
                "/api/books/{book_id}/chapters",
                &format!("/api/books/{book}/chapters"),
                200,
            )
            .await["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect();
        Fx {
            s,
            b,
            book,
            audiobook: ab["id"].as_str().unwrap().to_string(),
            chapters,
        }
    }

    async fn make_all(&self) {
        let j = self
            .s
            .post(
                "/api/audiobooks/{audiobook_id}/make-ready",
                &format!("/api/audiobooks/{}/make-ready", self.audiobook),
                json!({ "scope": { "kind": "whole_book" } }),
                202,
            )
            .await;
        self.wait_job(j["id"].as_str().unwrap()).await;
    }

    async fn wait_job(&self, id: &str) -> Value {
        for _ in 0..400 {
            let j = self
                .s
                .get("/api/jobs/{job_id}", &format!("/api/jobs/{id}"), 200)
                .await;
            if ["completed", "failed", "stopped", "needs_you"]
                .contains(&j["state"].as_str().unwrap())
            {
                return j;
            }
            tokio::time::sleep(StdDuration::from_millis(25)).await;
        }
        panic!("job {id} did not finish");
    }

    async fn chapter_states(&self) -> Vec<Value> {
        self.s
            .get(
                "/api/audiobooks/{audiobook_id}/chapters",
                &format!("/api/audiobooks/{}/chapters", self.audiobook),
                200,
            )
            .await["items"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn space_path(&self) -> String {
        format!("/api/audiobooks/{}/space", self.audiobook)
    }
}

#[tokio::test]
async fn freeing_space_deletes_the_audio_and_keeps_the_rest() {
    let f = Fx::new().await;
    // placed in the book, so there is something to keep
    let ch0 = f.chapters[0].clone();
    f.s.put(
        "/api/books/{book_id}/place",
        &format!("/api/books/{}/place", f.book),
        json!({ "chapter_id": ch0, "offset": 5, "mode": "listening", "base_revision": 0 }),
        200,
    )
    .await;
    let empty = f.s.get(SPACE, &f.space_path(), 200).await;
    assert_eq!(
        (
            empty["bytes"].as_i64().unwrap(),
            empty["chapters"].as_i64().unwrap()
        ),
        (0, 0)
    );
    assert_eq!(
        empty["remake_estimate"],
        Value::Null,
        "free voices have no remake cost"
    );

    f.b.state.lock().unwrap().delay_ms = 150;
    let j =
        f.s.post(
            "/api/audiobooks/{audiobook_id}/make-ready",
            &format!("/api/audiobooks/{}/make-ready", f.audiobook),
            json!({ "scope": { "kind": "whole_book" } }),
            202,
        )
        .await;
    let e = f.s.delete(SPACE, &f.space_path(), 409).await;
    assert_eq!(e["code"], "job_running");
    f.b.state.lock().unwrap().delay_ms = 0;
    f.wait_job(j["id"].as_str().unwrap()).await;

    let used = f.s.get(SPACE, &f.space_path(), 200).await;
    assert_eq!(used["chapters"], 3);
    let bytes = used["bytes"].as_i64().unwrap();
    assert!(bytes > 1000);
    let audio = f.chapter_states().await[0]["audio"].clone();
    let file = f.s.dir.path().join("audio").join(&f.audiobook);
    assert!(std::fs::read_dir(&file).unwrap().count() >= 3);

    let freed = f.s.delete(SPACE, &f.space_path(), 200).await;
    assert_eq!(freed["freed_bytes"], bytes);
    assert_eq!(
        std::fs::read_dir(&file).unwrap().count(),
        0,
        "the files are gone"
    );
    f.s.raw(
        "/api/audio/{audio_id}",
        audio["url"].as_str().unwrap(),
        &[],
        404,
    )
    .await;
    assert!(f
        .chapter_states()
        .await
        .iter()
        .all(|c| c["state"] == "not_yet"));
    assert_eq!(f.s.get(SPACE, &f.space_path(), 200).await["bytes"], 0);
    assert_eq!(
        f.s.delete(SPACE, &f.space_path(), 200).await["freed_bytes"],
        0,
        "nothing left to free"
    );

    // the book, the place and the audiobook are untouched, and it can be made again
    assert_eq!(
        f.s.get(
            "/api/books/{book_id}/place",
            &format!("/api/books/{}/place", f.book),
            200
        )
        .await["offset"],
        5
    );
    f.s.clock.advance(Duration::minutes(1));
    f.make_all().await;
    assert!(f
        .chapter_states()
        .await
        .iter()
        .all(|c| c["state"] == "ready"));
    f.s.delete(SPACE, "/api/audiobooks/nope/space", 404).await;
    f.s.stop().await;
}

#[tokio::test]
async fn the_manifest_lists_what_is_ready_with_the_hashes_a_device_needs() {
    let f = Fx::new().await;
    let mp = format!("/api/audiobooks/{}/manifest", f.audiobook);
    let tpl = "/api/audiobooks/{audiobook_id}/manifest";
    assert_eq!(
        f.s.get(tpl, &mp, 200).await["chapters"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let j = f.s.post("/api/audiobooks/{audiobook_id}/make-ready", &format!("/api/audiobooks/{}/make-ready", f.audiobook), json!({ "scope": { "kind": "chapters", "chapter_ids": [f.chapters[0], f.chapters[2]] } }), 202).await;
    f.wait_job(j["id"].as_str().unwrap()).await;
    let m = f.s.get(tpl, &mp, 200).await;
    let items = m["chapters"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .map(|c| c["chapter_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![f.chapters[0].as_str(), f.chapters[2].as_str()]
    );
    let listed =
        f.s.get(
            "/api/books/{book_id}/chapters",
            &format!("/api/books/{}/chapters", f.book),
            200,
        )
        .await;
    assert_eq!(items[1]["text_sha256"], listed["items"][2]["text_sha256"]);
    assert_eq!(items[0]["audio"]["content_type"], "audio/wav");
    f.s.get(tpl, "/api/audiobooks/nope/manifest", 404).await;
    f.s.stop().await;
}

#[tokio::test]
async fn a_device_is_told_what_changed_but_a_kept_copy_stays_valid() {
    let f = Fx::new().await;
    f.make_all().await;
    let st = f.chapter_states().await;
    let held = |i: usize| json!({ "chapter_id": f.chapters[i], "audio_id": st[i]["audio"]["id"] });
    let tpl = "/api/audiobooks/{audiobook_id}/sync-check";
    let path = format!("/api/audiobooks/{}/sync-check", f.audiobook);
    let all =
        f.s.post(
            tpl,
            &path,
            json!({ "have": [held(0), held(1), held(2)] }),
            200,
        )
        .await;
    assert_eq!(all["up_to_date"].as_array().unwrap().len(), 3);
    assert_eq!(all["newer"].as_array().unwrap().len(), 0);

    // free the space and make one chapter again later: the old copy is still described truthfully
    f.s.delete(SPACE, &f.space_path(), 200).await;
    assert_eq!(
        f.s.post(tpl, &path, json!({ "have": [held(0)] }), 200)
            .await["up_to_date"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "freed and not remade: the copy is not out of date"
    );
    f.s.clock.advance(Duration::minutes(5));
    let j =
        f.s.post(
            "/api/audiobooks/{audiobook_id}/make-ready",
            &format!("/api/audiobooks/{}/make-ready", f.audiobook),
            json!({ "scope": { "kind": "chapters", "chapter_ids": [f.chapters[0]] } }),
            202,
        )
        .await;
    f.wait_job(j["id"].as_str().unwrap()).await;
    let r =
        f.s.post(tpl, &path, json!({ "have": [held(0), held(1)] }), 200)
            .await;
    assert_eq!(r["up_to_date"], json!([f.chapters[1]]));
    let n = &r["newer"][0];
    assert_eq!(n["chapter_id"], f.chapters[0]);
    assert_eq!(n["old_audio_id"], st[0]["audio"]["id"]);
    assert_ne!(n["new_audio"]["id"], st[0]["audio"]["id"]);
    assert_eq!(n["changes"]["voice_name"], "Mara");
    assert_eq!(n["changes"]["old_bytes"], st[0]["audio"]["bytes"]);
    assert_eq!(
        n["changes"]["old_voice_revision"],
        n["changes"]["new_voice_revision"]
    );
    // nothing was replaced on the server's side by asking
    assert_eq!(
        f.chapter_states().await[0]["audio"]["id"],
        n["new_audio"]["id"]
    );

    f.s.post(
        tpl,
        &path,
        json!({ "have": [{ "chapter_id": "nope", "audio_id": "x" }] }),
        404,
    )
    .await;
    f.s.post(tpl, &path, json!({}), 400).await;
    f.s.stop().await;
}

#[tokio::test]
async fn deletion_hides_at_once_can_be_undone_and_then_runs_for_good() {
    let f = Fx::new().await;
    f.make_all().await;
    let ch0 = f.chapters[0].clone();
    f.s.put(
        "/api/books/{book_id}/place",
        &format!("/api/books/{}/place", f.book),
        json!({ "chapter_id": ch0, "offset": 9, "mode": "reading", "base_revision": 0 }),
        200,
    )
    .await;
    let dp = format!("/api/books/{}/deletion", f.book);
    f.s.get(DEL, &dp, 404).await;

    f.s.post(DEL, &dp, json!({ "delay_seconds": 10 }), 400)
        .await;
    let d = f.s.post(DEL, &dp, json!({}), 202).await;
    assert_eq!(d["state"], "pending");
    assert_eq!(d["scheduled_at"], "2026-01-15T12:00:00.000Z");
    assert_eq!(d["executes_at"], "2026-01-15T12:01:00.000Z");
    assert_eq!(d["scheduled_by"]["listener_name"], "Nick");
    f.s.get(
        "/api/books/{book_id}",
        &format!("/api/books/{}", f.book),
        404,
    )
    .await;
    let listed = f.s.get("/api/books", "/api/books", 200).await;
    assert_eq!(
        listed["items"].as_array().unwrap().len(),
        0,
        "hidden at once"
    );
    let e = f.s.post(DEL, &dp, json!({}), 409).await;
    assert_eq!(e["code"], "deletion_pending");
    assert_eq!(f.s.get(DEL, &dp, 200).await["state"], "pending");

    // undo: exactly as it was
    f.s.delete(DEL, &dp, 204).await;
    assert_eq!(
        f.s.get(
            "/api/books/{book_id}",
            &format!("/api/books/{}", f.book),
            200
        )
        .await["state"],
        "readable"
    );
    assert_eq!(
        f.s.get(
            "/api/books/{book_id}/place",
            &format!("/api/books/{}/place", f.book),
            200
        )
        .await["offset"],
        9
    );
    assert!(f
        .chapter_states()
        .await
        .iter()
        .all(|c| c["state"] == "ready"));
    assert_eq!(
        f.s.delete(DEL, &dp, 404).await["code"],
        "deletion_not_found"
    );
    f.s.get(DEL, &dp, 404).await;
    // a deletion that was cancelled never runs
    f.s.clock.advance(Duration::minutes(5));
    tokio::time::sleep(StdDuration::from_millis(1300)).await;
    f.s.get(
        "/api/books/{book_id}",
        &format!("/api/books/{}", f.book),
        200,
    )
    .await;

    // schedule again and let it run
    f.s.post(DEL, &dp, json!({ "delay_seconds": 60 }), 202)
        .await;
    tokio::time::sleep(StdDuration::from_millis(1300)).await;
    f.s.get(DEL, &dp, 200).await;
    assert_eq!(
        f.s.get(DEL, &dp, 200).await["state"],
        "pending",
        "not before its time"
    );
    f.s.clock.advance(Duration::seconds(61));
    let mut done = Value::Null;
    for _ in 0..80 {
        done = f.s.get(DEL, &dp, 200).await;
        if done["state"] == "done" {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    assert_eq!(done["state"], "done");
    f.s.get(
        "/api/books/{book_id}",
        &format!("/api/books/{}", f.book),
        404,
    )
    .await;
    assert_eq!(f.s.delete(DEL, &dp, 409).await["code"], "deletion_done");
    f.s.get(
        "/api/audiobooks/{audiobook_id}",
        &format!("/api/audiobooks/{}", f.audiobook),
        404,
    )
    .await;
    assert!(
        !f.s.dir.path().join("audio").join(&f.audiobook).exists(),
        "the audio files are gone"
    );
    assert!(
        !f.s.dir.path().join("originals").join(&f.book).exists()
            || std::fs::read_dir(f.s.dir.path().join("originals").join(&f.book))
                .unwrap()
                .count()
                == 0
    );
    assert_eq!(
        f.s.get(
            "/api/audit",
            "/api/audit?action=book.deletion_scheduled",
            200
        )
        .await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    f.s.post(DEL, &dp, json!({}), 404).await;
    f.s.stop().await;
}

#[tokio::test]
async fn deletion_returns_a_removed_book_to_removed_and_waits_for_running_jobs() {
    let f = Fx::new().await;
    let dp = format!("/api/books/{}/deletion", f.book);
    f.s.post(
        "/api/books/{book_id}/remove",
        &format!("/api/books/{}/remove", f.book),
        json!({}),
        200,
    )
    .await;
    f.s.post(DEL, &dp, json!({}), 202).await;
    f.s.delete(DEL, &dp, 204).await;
    assert_eq!(
        f.s.get(
            "/api/books/{book_id}",
            &format!("/api/books/{}", f.book),
            200
        )
        .await["state"],
        "removed"
    );
    f.s.post(
        "/api/books/{book_id}/restore",
        &format!("/api/books/{}/restore", f.book),
        json!({}),
        200,
    )
    .await;

    f.b.state.lock().unwrap().delay_ms = 200;
    let j =
        f.s.post(
            "/api/audiobooks/{audiobook_id}/make-ready",
            &format!("/api/audiobooks/{}/make-ready", f.audiobook),
            json!({ "scope": { "kind": "whole_book" } }),
            202,
        )
        .await;
    let e = f.s.post(DEL, &dp, json!({}), 409).await;
    assert_eq!(e["code"], "job_running");
    f.s.post(
        "/api/jobs/{job_id}/cancel",
        &format!("/api/jobs/{}/cancel", j["id"].as_str().unwrap()),
        json!({}),
        200,
    )
    .await;
    f.s.stop().await;
}

#[tokio::test]
async fn a_scheduled_deletion_survives_a_restart() {
    let f = Fx::new().await;
    let dp = format!("/api/books/{}/deletion", f.book);
    f.s.post(DEL, &dp, json!({}), 202).await;
    let (b, book, audiobook, chapters) = (f.b, f.book, f.audiobook, f.chapters);
    let dir = f.s.stop().await;
    let s = TestServer::start_with(dir, quick).await;
    let l = s.listener("Sam").await;
    s.act_as(&l);
    let f = Fx {
        s,
        b,
        book,
        audiobook,
        chapters,
    };
    assert_eq!(f.s.get(DEL, &dp, 200).await["state"], "pending");
    f.s.clock.advance(Duration::seconds(61));
    for _ in 0..80 {
        if f.s.get(DEL, &dp, 200).await["state"] == "done" {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    assert_eq!(f.s.get(DEL, &dp, 200).await["state"], "done");
    f.s.stop().await;
}

#[tokio::test]
async fn a_backup_is_a_consistent_database_copy_with_the_media_linked_in() {
    let f = Fx::new().await;
    f.make_all().await;
    // a source with a key, set directly: the backup must not carry it
    {
        let db = rusqlite::Connection::open(f.s.dir.path().join("bardic.db")).unwrap();
        db.busy_timeout(StdDuration::from_secs(5)).unwrap();
        db.execute(
            "UPDATE voice_sources SET config='{\"base_url\":\"http://10.0.0.5:7860\",\"api_key\":\"sekrit-key-123\"}' WHERE id='breeze'",
            [],
        )
        .unwrap();
    }
    assert_eq!(
        f.s.get("/api/backups", "/api/backups", 200).await["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let b =
        f.s.post("/api/backups", "/api/backups", json!({}), 202)
            .await;
    assert_eq!(b["state"], "running");
    let mut done = Value::Null;
    for _ in 0..200 {
        done = f.s.get("/api/backups", "/api/backups", 200).await["items"][0].clone();
        if done["state"] != "running" {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(25)).await;
    }
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["id"], b["id"]);
    // clients are told a place inside the data folder, not where the server keeps it
    assert_eq!(
        done["path"],
        format!("backups/{}", b["id"].as_str().unwrap())
    );
    let dir = f.s.dir.path().join(done["path"].as_str().unwrap());
    assert!(done["bytes"].as_i64().unwrap() > 1000);
    let raw = std::fs::read(dir.join("bardic.db")).unwrap();
    assert!(
        !raw.windows(14).any(|w| w == b"sekrit-key-123"),
        "the backup file still holds the key somewhere"
    );
    let live = rusqlite::Connection::open(f.s.dir.path().join("bardic.db")).unwrap();
    let kept: String = live
        .query_row(
            "SELECT config FROM voice_sources WHERE id='breeze'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        kept.contains("sekrit-key-123"),
        "the live database keeps its key"
    );

    // the copy opens, and holds the book
    let conn = rusqlite::Connection::open_with_flags(
        dir.join("bardic.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM books WHERE id=?1", [&f.book], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1);
    let audio_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM audio", [], |r| r.get(0))
        .unwrap();
    assert_eq!(audio_rows, 3);
    // and its media are the same files, not copies
    let live = f.s.dir.path().join("audio").join(&f.audiobook);
    let backed = dir.join("media").join("audio").join(&f.audiobook);
    let mut names: Vec<_> = std::fs::read_dir(&live)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names.len(), 3);
    for name in names {
        assert_eq!(
            std::fs::metadata(live.join(&name)).unwrap().len(),
            std::fs::metadata(backed.join(&name)).unwrap().len()
        );
    }
    f.s.stop().await;
}

fn stub(dir: &std::path::Path, fail: bool) -> String {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("ffmpeg-call");
    let body = format!(
        "#!/bin/sh\n[ \"$1\" = \"-version\" ] && exit 0\nn=0; prev=\"\"; last=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-i\" ]; then n=$((n+1)); [ $n -eq 1 ] && cp \"$a\" \"{log}.list\"; [ $n -eq 2 ] && cp \"$a\" \"{log}.meta\"; fi\n  prev=\"$a\"; last=\"$a\"\ndone\n{}\necho \"$@\" > \"{log}.args\"\nprintf FAKEM4B > \"$last\"\n",
        if fail { "exit 1" } else { "" },
        log = log.display()
    );
    let path = dir.join("fake-ffmpeg");
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().to_string()
}

#[tokio::test]
async fn an_export_encodes_the_ready_chapters_with_markers_and_can_be_downloaded() {
    let tools = tempfile::tempdir().unwrap();
    let ff = stub(tools.path(), false);
    let f = Fx::with(move |c| c.ffmpeg = ff).await;
    let ex = format!("/api/audiobooks/{}/exports", f.audiobook);
    let tpl = "/api/audiobooks/{audiobook_id}/exports";
    let e =
        f.s.call(Method::POST, tpl, &ex, Some(DEVICE), Some(json!({})), 409)
            .await;
    assert_eq!(e["code"], "nothing_ready");

    let j =
        f.s.post(
            "/api/audiobooks/{audiobook_id}/make-ready",
            &format!("/api/audiobooks/{}/make-ready", f.audiobook),
            json!({ "scope": { "kind": "from_chapter", "from_chapter_id": f.chapters[1] } }),
            202,
        )
        .await;
    f.wait_job(j["id"].as_str().unwrap()).await;
    f.s.call(
        Method::POST,
        tpl,
        &ex,
        Some(DEVICE),
        Some(json!({ "format": "mp3" })),
        400,
    )
    .await;
    let e =
        f.s.call(Method::POST, tpl, &ex, Some(DEVICE), None, 202)
            .await;
    assert_eq!(
        (e["state"].as_str().unwrap(), e["format"].as_str().unwrap()),
        ("running", "m4b")
    );
    let id = e["id"].as_str().unwrap().to_string();
    let mut ready = Value::Null;
    for _ in 0..200 {
        ready =
            f.s.get(
                "/api/exports/{export_id}",
                &format!("/api/exports/{id}"),
                200,
            )
            .await;
        if ready["state"] != "running" {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(25)).await;
    }
    assert_eq!(ready["state"], "ready", "{ready}");
    assert_eq!(ready["bytes"], 7);
    let job =
        f.s.get(
            "/api/jobs/{job_id}",
            &format!("/api/jobs/{}", ready["job_id"].as_str().unwrap()),
            200,
        )
        .await;
    assert_eq!(
        (
            job["kind"].as_str().unwrap(),
            job["state"].as_str().unwrap()
        ),
        ("export", "completed")
    );

    let (h, body) =
        f.s.raw(
            "/api/exports/{export_id}/file",
            &format!("/api/exports/{id}/file"),
            &[],
            200,
        )
        .await;
    assert_eq!(body, b"FAKEM4B");
    assert_eq!(h["content-type"], "audio/mp4");
    assert!(h["content-disposition"]
        .to_str()
        .unwrap()
        .contains("The Lantern Keeper.m4b"));
    f.s.raw(
        "/api/exports/{export_id}/file",
        &format!("/api/exports/{id}/file"),
        &[("range", "bytes=0-3")],
        206,
    )
    .await;

    // two chapters were ready, so the file has two chapter markers that add up to the audio
    let meta = std::fs::read_to_string(tools.path().join("ffmpeg-call.meta")).unwrap();
    assert_eq!(meta.matches("[CHAPTER]").count(), 2);
    assert!(meta.contains("title=The Lantern Keeper") && meta.contains("START=0"));
    let list = std::fs::read_to_string(tools.path().join("ffmpeg-call.list")).unwrap();
    assert_eq!(list.lines().count(), 2);
    f.s.get("/api/exports/{export_id}", "/api/exports/nope", 404)
        .await;
    f.s.raw(
        "/api/exports/{export_id}/file",
        "/api/exports/nope/file",
        &[],
        404,
    )
    .await;
    f.s.stop().await;
}

#[tokio::test]
async fn an_export_that_cannot_run_says_so() {
    let tools = tempfile::tempdir().unwrap();
    let ff = stub(tools.path(), true);
    let f = Fx::with(move |c| c.ffmpeg = ff).await;
    f.make_all().await;
    let ex = format!("/api/audiobooks/{}/exports", f.audiobook);
    let tpl = "/api/audiobooks/{audiobook_id}/exports";
    let e =
        f.s.call(Method::POST, tpl, &ex, Some(DEVICE), None, 202)
            .await;
    let id = e["id"].as_str().unwrap().to_string();
    let mut st = Value::Null;
    for _ in 0..200 {
        st =
            f.s.get(
                "/api/exports/{export_id}",
                &format!("/api/exports/{id}"),
                200,
            )
            .await;
        if st["state"] != "running" {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(25)).await;
    }
    assert_eq!(st["state"], "failed");
    let job =
        f.s.get(
            "/api/jobs/{job_id}",
            &format!("/api/jobs/{}", st["job_id"].as_str().unwrap()),
            200,
        )
        .await;
    assert_eq!(job["state"], "failed");
    assert_eq!(job["needs_you"]["code"], "export_failed");
    let e =
        f.s.raw(
            "/api/exports/{export_id}/file",
            &format!("/api/exports/{id}/file"),
            &[],
            409,
        )
        .await;
    assert!(String::from_utf8_lossy(&e.1).contains("export_not_ready"));
    f.s.stop().await;

    // no ffmpeg at all
    let f = Fx::with(|c| c.ffmpeg = "/no/such/ffmpeg".into()).await;
    f.make_all().await;
    let e =
        f.s.call(
            Method::POST,
            tpl,
            &format!("/api/audiobooks/{}/exports", f.audiobook),
            Some(DEVICE),
            None,
            409,
        )
        .await;
    assert_eq!(e["code"], "encoder_missing");
    f.s.stop().await;
}

#[tokio::test]
async fn a_premium_audiobook_says_what_making_it_again_would_cost() {
    let g = FakeGemini::start("k").await;
    let url = g.url.clone();
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), move |c| {
        quick(c);
        c.gemini_url = url;
    })
    .await;
    let l = s.listener("Nick").await;
    s.act_as(&l);
    s.put(
        SRC,
        "/api/voice-sources/gemini",
        json!({ "api_key": "k" }),
        200,
    )
    .await;
    let book = s
        .post("/api/books/sample", "/api/books/sample", json!({}), 201)
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let voice = s.get("/api/voices", "/api/voices", 200).await["items"][0]["id"].clone();
    let ab = s
        .post(
            "/api/books/{book_id}/audiobooks",
            &format!("/api/books/{book}/audiobooks"),
            json!({ "voice_id": voice }),
            201,
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let sp = s
        .get(SPACE, &format!("/api/audiobooks/{ab}/space"), 200)
        .await;
    assert_eq!(
        sp["remake_estimate"]["likely"]["micros"], 0,
        "nothing made yet, nothing to remake"
    );
    assert_eq!(sp["remake_estimate"]["basis"], "manual");
    s.stop().await;
}

/// Runs the real ffmpeg (skipped when it is not installed): the file must be a real M4B whose
/// chapter markers and duration match the chapters that were exported.
#[tokio::test]
async fn a_real_ffmpeg_makes_a_playable_m4b_with_chapter_markers() {
    let ffmpeg = std::env::var("BARDIC_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());
    let ffprobe = std::env::var("BARDIC_FFPROBE").unwrap_or_else(|_| "ffprobe".to_string());
    let have = |p: &str| {
        std::process::Command::new(p)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !have(&ffmpeg) || !have(&ffprobe) {
        eprintln!("skipped: ffmpeg and ffprobe are not installed");
        return;
    }
    let f = Fx::with(move |c| c.ffmpeg = ffmpeg).await;
    f.make_all().await;
    let ex = format!("/api/audiobooks/{}/exports", f.audiobook);
    let e =
        f.s.call(
            Method::POST,
            "/api/audiobooks/{audiobook_id}/exports",
            &ex,
            Some(DEVICE),
            None,
            202,
        )
        .await;
    let id = e["id"].as_str().unwrap().to_string();
    let mut ready = Value::Null;
    for _ in 0..400 {
        ready =
            f.s.get(
                "/api/exports/{export_id}",
                &format!("/api/exports/{id}"),
                200,
            )
            .await;
        if ready["state"] != "running" {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    assert_eq!(ready["state"], "ready", "{ready}");
    let (_, body) =
        f.s.raw(
            "/api/exports/{export_id}/file",
            &format!("/api/exports/{id}/file"),
            &[],
            200,
        )
        .await;
    let out = f.s.dir.path().join("probe.m4b");
    std::fs::write(&out, &body).unwrap();
    let probe = std::process::Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-show_chapters",
            "-of",
            "json",
        ])
        .arg(&out)
        .output()
        .unwrap();
    assert!(probe.status.success(), "ffprobe could not read the export");
    let j: Value = serde_json::from_slice(&probe.stdout).unwrap();
    eprintln!(
        "export: {} bytes, {}",
        body.len(),
        j["format"]["format_name"]
    );
    assert!(j["format"]["format_name"].as_str().unwrap().contains("mp4"));
    let audio: Vec<&Value> = j["streams"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .collect();
    assert_eq!(audio.len(), 1);
    assert_eq!(audio[0]["codec_name"], "aac");
    assert_eq!(audio[0]["channels"], 1);
    let chapters = j["chapters"].as_array().unwrap();
    assert_eq!(chapters.len(), f.chapters.len(), "{}", j["chapters"]);
    let mut prev_end = 0.0;
    for c in chapters {
        let (a, b) = (
            c["start_time"].as_str().unwrap().parse::<f64>().unwrap(),
            c["end_time"].as_str().unwrap().parse::<f64>().unwrap(),
        );
        assert!(
            a >= prev_end - 0.05 && b > a,
            "chapters overlap: {chapters:?}"
        );
        prev_end = b;
    }
    let dur: f64 = j["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!(
        (dur - prev_end).abs() < 0.5,
        "chapters end at {prev_end}s but the file is {dur}s"
    );
    f.s.stop().await;
}

#[tokio::test]
async fn asking_for_an_export_or_a_backup_again_returns_the_one_under_way() {
    use std::os::unix::fs::PermissionsExt;
    let tools = tempfile::tempdir().unwrap();
    let path = tools.path().join("slow-ffmpeg");
    std::fs::write(
        &path,
        "#!/bin/sh\n[ \"$1\" = \"-version\" ] && exit 0\nsleep 1\nfor a in \"$@\"; do last=\"$a\"; done\nprintf M4B > \"$last\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ff = path.to_string_lossy().to_string();
    let f = Fx::with(move |c| c.ffmpeg = ff).await;
    f.make_all().await;
    let ex = format!("/api/audiobooks/{}/exports", f.audiobook);
    let tpl = "/api/audiobooks/{audiobook_id}/exports";
    let a =
        f.s.call(Method::POST, tpl, &ex, Some(DEVICE), None, 202)
            .await;
    let b =
        f.s.call(Method::POST, tpl, &ex, Some(DEVICE), None, 202)
            .await;
    assert_eq!(
        a["id"], b["id"],
        "the second request must not start a second ffmpeg"
    );
    let id = a["id"].as_str().unwrap().to_string();
    for _ in 0..200 {
        let e =
            f.s.get(
                "/api/exports/{export_id}",
                &format!("/api/exports/{id}"),
                200,
            )
            .await;
        if e["state"] != "running" {
            assert_eq!(e["state"], "ready");
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    // once it has finished a new export is a new one
    let c =
        f.s.call(Method::POST, tpl, &ex, Some(DEVICE), None, 202)
            .await;
    assert_ne!(c["id"], a["id"]);

    // a backup recorded as running (as if one were under way) is returned, not doubled
    let db = rusqlite::Connection::open(f.s.dir.path().join("bardic.db")).unwrap();
    db.busy_timeout(StdDuration::from_secs(5)).unwrap();
    db.execute("UPDATE backups SET state='done' WHERE state='running'", [])
        .unwrap();
    db.execute(
        "INSERT INTO backups(id,state,created_at) VALUES('01RUNNINGBACKUP','running','2026-01-15T11:00:00.000Z')",
        [],
    )
    .unwrap();
    let r =
        f.s.call(
            Method::POST,
            "/api/backups",
            "/api/backups",
            Some(DEVICE),
            None,
            202,
        )
        .await;
    assert_eq!(r["id"], "01RUNNINGBACKUP");
    let n: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM backups WHERE state='running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    f.s.stop().await;
}

#[tokio::test]
async fn no_audio_is_made_for_a_book_scheduled_for_deletion() {
    let f = Fx::new().await;
    let dp = format!("/api/books/{}/deletion", f.book);
    f.s.post(DEL, &dp, json!({}), 202).await;
    let e =
        f.s.post(
            "/api/audiobooks/{audiobook_id}/make-ready",
            &format!("/api/audiobooks/{}/make-ready", f.audiobook),
            json!({ "scope": { "kind": "whole_book" } }),
            409,
        )
        .await;
    assert_eq!(e["code"], "deletion_pending", "{e}");
    let e =
        f.s.call(
            Method::POST,
            "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request",
            &format!(
                "/api/audiobooks/{}/chapters/{}/request",
                f.audiobook, f.chapters[0]
            ),
            Some(DEVICE),
            Some(json!({ "ahead": 0 })),
            409,
        )
        .await;
    assert_eq!(e["code"], "deletion_pending", "{e}");
    // after the undo it works again
    f.s.delete(DEL, &dp, 204).await;
    f.make_all().await;
    f.s.stop().await;
}

#[tokio::test]
async fn audio_folders_that_belong_to_no_audiobook_are_swept_and_real_ones_kept() {
    let f = Fx::new().await;
    f.make_all().await;
    let real = f.s.dir.path().join("audio").join(&f.audiobook);
    let orphan = f.s.dir.path().join("audio").join("01ORPHANAUDIOBOOK");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("01CHAPTER.part"), b"left behind").unwrap();
    let before = std::fs::read_dir(&real).unwrap().count();
    assert!(before > 0);
    let dir = f.s.stop().await;
    // a restart sweeps
    let s = TestServer::start_in(dir).await;
    for _ in 0..100 {
        if !orphan.exists() {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    assert!(!orphan.exists(), "the orphan folder is still there");
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), before);
    s.stop().await;
}
