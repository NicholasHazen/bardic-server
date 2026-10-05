//! Chapter display metadata can be refreshed without rewriting immutable book resources.
//! Fixtures are synthetic; every HTTP response is checked against the contract.
mod common;

use common::{breeze::FakeBreeze, epub::Epub, TestServer, DEVICE};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::Duration;

const CHAPTERS: &str = "/api/books/{book_id}/chapters";
const REFRESH: &str = "/api/books/{book_id}/chapters/refresh";
const BOOK: &str = "/api/books/{book_id}";
const TEXT: &str = "/api/books/{book_id}/chapters/{chapter_id}/text";

fn matter_epub() -> Epub {
    Epub {
        cover: None,
        chapters: vec![
            ("Copyright", "This synthetic ferry story is a test."),
            (
                "Chapter 1: Lantern River",
                "Zoë watched the lantern. A boat drifted past.",
            ),
            (
                "Chapter 2: Far Shore",
                "A bell rang across the water. Then silence.",
            ),
            ("Acknowledgements", "Thanks to the imaginary ferrymen."),
        ],
        ..Epub::default()
    }
}

async fn chapters(s: &TestServer, book: &str, query: &str) -> Value {
    s.get(CHAPTERS, &format!("/api/books/{book}/chapters{query}"), 200)
        .await
}

async fn text(s: &TestServer, book: &str, chapter: &Value) -> Value {
    s.get(
        TEXT,
        &format!(
            "/api/books/{book}/chapters/{}/text",
            chapter["id"].as_str().unwrap()
        ),
        200,
    )
    .await
}

async fn old_metadata(s: &TestServer, book: &str) {
    let book = book.to_string();
    s.running.as_ref().unwrap().state.store.run(move |c| {
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE chapters SET title='Section ' || (idx+1),kind='story' WHERE book_id=?1",
            [&book],
        )?;
        tx.execute(
            "UPDATE books SET story_chapter_count=chapter_count,word_count=(SELECT SUM(word_count) FROM chapters WHERE book_id=?1) WHERE id=?1",
            [&book],
        )?;
        tx.commit()?;
        Ok(())
    }).await.unwrap();
}

async fn updates(s: &TestServer) -> Value {
    s.get("/api/audit", "/api/audit?action=book.updated", 200)
        .await
}

#[tokio::test]
async fn hiding_matter_preserves_indices_ids_and_direct_text_access() {
    let s = TestServer::start().await;
    let book = s.add_book("matter.epub", matter_epub().build()).await;
    let all = chapters(&s, &book, "").await;
    assert_eq!(all["items"].as_array().unwrap().len(), 4);
    assert_eq!(chapters(&s, &book, "?include_matter=true").await, all);
    let hidden = chapters(&s, &book, "?include_matter=false").await;
    assert_eq!(hidden["items"], json!([all["items"][1], all["items"][2]]));
    assert_eq!(hidden["items"][0]["index"], 1);
    assert_eq!(hidden["items"][1]["index"], 2);
    for chapter in [&all["items"][0], &all["items"][3]] {
        let exact = text(&s, &book, chapter).await;
        assert_eq!(exact["chapter_id"], chapter["id"]);
        assert_eq!(exact["text_sha256"], chapter["text_sha256"]);
    }
    let invalid = s
        .get(
            CHAPTERS,
            &format!("/api/books/{book}/chapters?include_matter=perhaps"),
            400,
        )
        .await;
    assert_eq!(invalid["code"], "invalid_request");
    s.get(
        CHAPTERS,
        "/api/books/missing/chapters?include_matter=false",
        404,
    )
    .await;
    s.stop().await;
}

#[tokio::test]
async fn refresh_restores_names_kinds_and_counts_without_changing_text_places_or_audio() {
    let s = TestServer::start().await;
    let listener = s.listener("Metadata listener").await;
    s.act_as(&listener);
    let book = s.add_book("matter.epub", matter_epub().build()).await;
    let expected = chapters(&s, &book, "").await;
    let mut texts = Vec::new();
    for chapter in expected["items"].as_array().unwrap() {
        texts.push(text(&s, &book, chapter).await);
    }
    s.patch(BOOK, &format!("/api/books/{book}"), json!({ "title": "My ferry", "author": "My writer", "series": { "name": "River", "order": 2 } }), 200).await;
    let expected_book = s.get(BOOK, &format!("/api/books/{book}"), 200).await;

    let breeze = FakeBreeze::start().await;
    s.put(
        "/api/voice-sources/{source_id}",
        "/api/voice-sources/breeze",
        json!({ "base_url": breeze.url }),
        200,
    )
    .await;
    let voices = s.get("/api/voices", "/api/voices", 200).await;
    let voice = voices["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "Mara")
        .unwrap();
    let audiobook = s
        .post(
            "/api/books/{book_id}/audiobooks",
            &format!("/api/books/{book}/audiobooks"),
            json!({ "voice_id": voice["id"] }),
            201,
        )
        .await;
    let ab = audiobook["id"].as_str().unwrap();
    let ch = expected["items"][0]["id"].as_str().unwrap();
    let request_path = format!("/api/audiobooks/{ab}/chapters/{ch}/request");
    let request_template = "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request";
    let job = s
        .post(request_template, &request_path, json!({ "ahead": 0 }), 202)
        .await;
    let job_id = job["id"].as_str().unwrap();
    let job_path = format!("/api/jobs/{job_id}");
    let mut completed = None;
    for _ in 0..200 {
        let current = s.get("/api/jobs/{job_id}", &job_path, 200).await;
        if current["state"] == "completed" {
            completed = Some(current);
            break;
        }
        assert_ne!(current["state"], "failed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let completed = completed.expect("chapter audio finished");
    let ready = s
        .post(request_template, &request_path, json!({}), 200)
        .await;
    let audio = ready["audio"].clone();
    let (_, bytes) = s
        .raw(
            "/api/audio/{audio_id}",
            audio["url"].as_str().unwrap(),
            &[],
            200,
        )
        .await;
    let timings = s
        .get(
            "/api/audio/{audio_id}/timings",
            audio["timings_url"].as_str().unwrap(),
            200,
        )
        .await;

    old_metadata(&s, &book).await;
    let place_path = format!("/api/books/{book}/place");
    let first = s.put("/api/books/{book_id}/place", &place_path, json!({ "chapter_id": expected["items"][1]["id"], "offset": 3, "mode": "reading", "audiobook_id": null, "base_revision": 0 }), 200).await;
    let place = s.put("/api/books/{book_id}/place", &place_path, json!({ "chapter_id": expected["items"][2]["id"], "offset": 2, "mode": "reading", "audiobook_id": null, "base_revision": first["revision"] }), 200).await;
    let history_path = format!("/api/books/{book}/place/history");
    let history = s
        .get("/api/books/{book_id}/place/history", &history_path, 200)
        .await;
    let audit_before = updates(&s).await;
    let state = &s.running.as_ref().unwrap().state;
    let mut subscription = state.events.subscribe(None);

    let refreshed = s
        .post(
            REFRESH,
            &format!("/api/books/{book}/chapters/refresh"),
            json!({}),
            200,
        )
        .await;
    assert_eq!(refreshed, expected);
    let (_, notice) = subscription.rx.try_recv().expect("metadata notice");
    assert_eq!(notice.kind, "book.updated");
    assert_eq!(notice.book_id.as_deref(), Some(book.as_str()));
    assert!(subscription.rx.try_recv().is_err());
    let audit_after = updates(&s).await;
    assert_eq!(
        audit_after["items"].as_array().unwrap().len(),
        audit_before["items"].as_array().unwrap().len() + 1
    );
    let audit = &audit_after["items"][0];
    assert_eq!(audit["actor"]["device_id"], DEVICE);
    assert_eq!(audit["actor"]["listener_id"], Value::Null);
    assert_eq!(audit["target"]["book_id"], book);
    for (i, chapter) in refreshed["items"].as_array().unwrap().iter().enumerate() {
        assert_eq!(text(&s, &book, chapter).await, texts[i]);
    }
    assert_eq!(
        s.get("/api/books/{book_id}/place", &place_path, 200).await,
        place
    );
    assert_eq!(
        s.get("/api/books/{book_id}/place/history", &history_path, 200)
            .await,
        history
    );
    assert_eq!(s.get("/api/jobs/{job_id}", &job_path, 200).await, completed);
    assert_eq!(
        s.post(request_template, &request_path, json!({}), 200)
            .await["audio"],
        audio
    );
    assert_eq!(
        s.get(
            "/api/audio/{audio_id}/timings",
            audio["timings_url"].as_str().unwrap(),
            200
        )
        .await,
        timings
    );
    let (_, kept) = s
        .raw(
            "/api/audio/{audio_id}",
            audio["url"].as_str().unwrap(),
            &[],
            200,
        )
        .await;
    assert_eq!(kept, bytes);
    let refreshed_book = s.get(BOOK, &format!("/api/books/{book}"), 200).await;
    for field in [
        "title",
        "author",
        "series",
        "cover",
        "chapter_count",
        "story_chapter_count",
        "word_count",
        "source_sha256",
    ] {
        assert_eq!(refreshed_book[field], expected_book[field], "{field}");
    }

    assert_eq!(
        s.post(
            REFRESH,
            &format!("/api/books/{book}/chapters/refresh"),
            json!({}),
            200
        )
        .await,
        refreshed
    );
    assert_eq!(updates(&s).await, audit_after);
    assert!(
        subscription.rx.try_recv().is_err(),
        "no-op must not publish a notice"
    );
    s.stop().await;
    breeze.stop();
}

#[tokio::test]
async fn count_text_or_order_mismatch_never_partially_updates_metadata() {
    let s = TestServer::start().await;
    let listener = s.listener("Structure listener").await;
    s.act_as(&listener);
    let book = s.add_book("structure.epub", matter_epub().build()).await;
    old_metadata(&s, &book).await;
    let before = chapters(&s, &book, "").await;
    let before_book = s.get(BOOK, &format!("/api/books/{book}"), 200).await;
    let audit_before = updates(&s).await;
    let source = s
        .dir
        .path()
        .join("originals")
        .join(&book)
        .join("source.epub");
    let mut variants = Vec::new();
    let mut changed_count = matter_epub();
    changed_count.chapters.pop();
    variants.push(changed_count);
    let mut changed_text = matter_epub();
    changed_text.chapters[3].1 = "The final words differ.";
    variants.push(changed_text);
    let mut changed_order = matter_epub();
    changed_order.chapters.swap(1, 2);
    variants.push(changed_order);
    let mut subscription = s.running.as_ref().unwrap().state.events.subscribe(None);
    for epub in variants {
        std::fs::write(&source, epub.build()).unwrap();
        let error = s
            .post(
                REFRESH,
                &format!("/api/books/{book}/chapters/refresh"),
                json!({}),
                409,
            )
            .await;
        assert_eq!(error["code"], "chapter_structure_changed");
        assert_eq!(chapters(&s, &book, "").await, before);
        assert_eq!(
            s.get(BOOK, &format!("/api/books/{book}"), 200).await,
            before_book
        );
        assert_eq!(updates(&s).await, audit_before);
        assert!(subscription.rx.try_recv().is_err());
    }
    s.stop().await;
}

#[tokio::test]
async fn unavailable_original_is_a_conflict_and_keeps_metadata() {
    let s = TestServer::start().await;
    let book = s.add_book("unavailable.epub", matter_epub().build()).await;
    old_metadata(&s, &book).await;
    let before = chapters(&s, &book, "").await;
    let source = s
        .dir
        .path()
        .join("originals")
        .join(&book)
        .join("source.epub");
    std::fs::write(&source, b"This is not an EPUB.").unwrap();
    let invalid = s
        .post(
            REFRESH,
            &format!("/api/books/{book}/chapters/refresh"),
            json!({}),
            409,
        )
        .await;
    assert_eq!(invalid["code"], "source_unavailable");
    assert_eq!(chapters(&s, &book, "").await, before);
    std::fs::remove_file(source).unwrap();
    let missing = s
        .post(
            REFRESH,
            &format!("/api/books/{book}/chapters/refresh"),
            json!({}),
            409,
        )
        .await;
    assert_eq!(missing["code"], "source_unavailable");
    assert_eq!(chapters(&s, &book, "").await, before);
    let sample = s
        .post("/api/books/sample", "/api/books/sample", json!({}), 201)
        .await;
    let unavailable = s
        .post(
            REFRESH,
            &format!(
                "/api/books/{}/chapters/refresh",
                sample["id"].as_str().unwrap()
            ),
            json!({}),
            409,
        )
        .await;
    assert_eq!(unavailable["code"], "source_unavailable");
    assert_eq!(updates(&s).await["items"], json!([]));
    s.stop().await;
}

#[tokio::test]
async fn refresh_requires_a_device_and_an_editable_book() {
    let s = TestServer::start().await;
    let book = s.add_book("states.epub", matter_epub().build()).await;
    let path = format!("/api/books/{book}/chapters/refresh");
    let error = s
        .call(Method::POST, REFRESH, &path, None, Some(json!({})), 400)
        .await;
    assert_eq!(error["code"], "device_required");
    s.post(
        REFRESH,
        "/api/books/missing/chapters/refresh",
        json!({}),
        404,
    )
    .await;
    for (state, code) in [
        ("adding", "book_adding"),
        ("removed", "book_removed"),
        ("deleting", "deletion_pending"),
    ] {
        let target = book.clone();
        s.running
            .as_ref()
            .unwrap()
            .state
            .store
            .run(move |c| {
                c.execute("UPDATE books SET state=?2 WHERE id=?1", [&target, state])?;
                Ok(())
            })
            .await
            .unwrap();
        let error = s.post(REFRESH, &path, json!({}), 409).await;
        assert_eq!(error["code"], code);
    }
    s.stop().await;
}
