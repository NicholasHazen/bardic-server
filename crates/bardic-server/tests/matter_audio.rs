//! Chapter matter stays readable while audio scopes may exclude it. Providers are local fakes.
mod common;

use common::{
    breeze::FakeBreeze,
    epub::Epub,
    gemini::{money, FakeGemini},
    TestServer,
};
use serde_json::{json, Value};
use std::time::Duration;

const READY: &str = "/api/audiobooks/{audiobook_id}/make-ready";
const PREVIEW: &str = "/api/audiobooks/{audiobook_id}/plan-preview";
const REQUEST: &str = "/api/audiobooks/{audiobook_id}/chapters/{chapter_id}/request";

struct Fx {
    s: TestServer,
    book: String,
    audiobook: String,
    chapters: Vec<String>,
    texts: Vec<Value>,
}

impl Fx {
    async fn new(s: TestServer, source: &str, config: Value, voice_name: &str) -> Self {
        Self::with_chapters(
            s,
            source,
            config,
            voice_name,
            vec![
                ("Copyright", "This synthetic edition belongs to nobody."),
                (
                    "Chapter One",
                    "The lantern glowed beside the quiet shore. 🏮",
                ),
                ("Chapter Two", "Mira rowed through the fog toward home."),
                ("Acknowledgements", "Thanks to the imaginary ferrymakers."),
            ],
            &["front_matter", "story", "story", "back_matter"],
        )
        .await
    }

    async fn with_chapters(
        s: TestServer,
        source: &str,
        config: Value,
        voice_name: &str,
        chapters: Vec<(&'static str, &'static str)>,
        kinds: &[&str],
    ) -> Self {
        let listener = s.listener("Reader").await;
        s.act_as(&listener);
        s.put(
            "/api/voice-sources/{source_id}",
            &format!("/api/voice-sources/{source}"),
            config,
            200,
        )
        .await;
        let book = s
            .add_book(
                "matter.epub",
                Epub {
                    title: "The Lantern Test",
                    cover: None,
                    chapters,
                    ..Default::default()
                }
                .build(),
            )
            .await;
        let listed = s
            .get(
                "/api/books/{book_id}/chapters",
                &format!("/api/books/{book}/chapters"),
                200,
            )
            .await;
        assert_eq!(
            listed["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            kinds,
        );
        let chapters: Vec<String> = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect();
        let mut texts = vec![];
        for id in &chapters {
            texts.push(
                s.get(
                    "/api/books/{book_id}/chapters/{chapter_id}/text",
                    &format!("/api/books/{book}/chapters/{id}/text"),
                    200,
                )
                .await,
            );
        }
        let voices = s.get("/api/voices", "/api/voices", 200).await;
        let voice = voices["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == voice_name)
            .unwrap();
        let ab = s
            .post(
                "/api/books/{book_id}/audiobooks",
                &format!("/api/books/{book}/audiobooks"),
                json!({ "voice_id": voice["id"] }),
                201,
            )
            .await;
        Self {
            s,
            book,
            audiobook: ab["id"].as_str().unwrap().to_string(),
            chapters,
            texts,
        }
    }

    async fn make_ready(&self, scope: Value) -> Value {
        self.s
            .post(
                READY,
                &format!("/api/audiobooks/{}/make-ready", self.audiobook),
                json!({ "scope": scope }),
                202,
            )
            .await
    }

    async fn preview(&self, scope: Value, expected: u16) -> Value {
        self.s
            .post(
                PREVIEW,
                &format!("/api/audiobooks/{}/plan-preview", self.audiobook),
                json!({ "scope": scope }),
                expected,
            )
            .await
    }

    async fn approve(&self, preview: &Value, expected: u16) -> Value {
        self.s
            .post(
                "/api/plans",
                "/api/plans",
                json!({ "estimate_id": preview["estimate_id"], "limit": preview["suggested_limit"] }),
                expected,
            )
            .await
    }

    async fn wait(&self, kind: &str, id: &str) -> Value {
        let template = if kind == "jobs" {
            "/api/jobs/{job_id}"
        } else {
            "/api/plans/{plan_id}"
        };
        for _ in 0..400 {
            let value = self
                .s
                .get(template, &format!("/api/{kind}/{id}"), 200)
                .await;
            if value["state"] == "completed" {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{kind}/{id} did not complete");
    }

    async fn states(&self) -> Vec<String> {
        self.s
            .get(
                "/api/audiobooks/{audiobook_id}/chapters",
                &format!("/api/audiobooks/{}/chapters", self.audiobook),
                200,
            )
            .await["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["state"].as_str().unwrap().to_string())
            .collect()
    }

    fn characters(&self, indices: &[usize]) -> i64 {
        indices
            .iter()
            .flat_map(|i| self.texts[*i]["lines"].as_array().unwrap())
            .map(|l| l["end"].as_i64().unwrap() - l["start"].as_i64().unwrap())
            .sum()
    }
}

async fn free() -> (Fx, FakeBreeze) {
    let b = FakeBreeze::start().await;
    let s = TestServer::start().await;
    let f = Fx::new(s, "breeze", json!({ "base_url": b.url }), "Mara").await;
    (f, b)
}

async fn premium() -> (Fx, FakeGemini) {
    let g = FakeGemini::start("synthetic-key").await;
    let s = TestServer::start_with(tempfile::tempdir().unwrap(), |c| {
        c.gemini_url = g.url.clone();
    })
    .await;
    let f = Fx::new(s, "gemini", json!({ "api_key": "synthetic-key" }), "Kore").await;
    (f, g)
}

#[tokio::test]
async fn free_audio_can_exclude_matter_then_add_it_without_replacing_finished_audio() {
    let (f, b) = free().await;
    let job = f
        .make_ready(json!({ "kind": "whole_book", "include_matter": false }))
        .await;
    assert_eq!(job["chapters_total"], 2);
    let done = f.wait("jobs", job["id"].as_str().unwrap()).await;
    assert_eq!(done["chapters_done"], 2);
    assert_eq!(f.states().await, ["not_yet", "ready", "ready", "not_yet"]);
    let spoken = b.spoken().join("\n");
    assert!(spoken.contains("🏮") && spoken.contains("Mira rowed"));
    assert!(!spoken.contains("synthetic edition") && !spoken.contains("ferrymakers"));

    let before =
        f.s.get(
            "/api/audiobooks/{audiobook_id}/chapters",
            &format!("/api/audiobooks/{}/chapters", f.audiobook),
            200,
        )
        .await;
    let job = f
        .make_ready(json!({ "kind": "whole_book", "include_matter": true }))
        .await;
    assert_eq!(job["chapters_total"], 2, "only the omitted matter remains");
    f.wait("jobs", job["id"].as_str().unwrap()).await;
    let after =
        f.s.get(
            "/api/audiobooks/{audiobook_id}/chapters",
            &format!("/api/audiobooks/{}/chapters", f.audiobook),
            200,
        )
        .await;
    for i in 1..=2 {
        assert_eq!(before["items"][i]["audio"], after["items"][i]["audio"]);
    }
    assert_eq!(f.states().await, ["ready", "ready", "ready", "ready"]);
    for (i, id) in f.chapters.iter().enumerate() {
        let text =
            f.s.get(
                "/api/books/{book_id}/chapters/{chapter_id}/text",
                &format!("/api/books/{}/chapters/{id}/text", f.book),
                200,
            )
            .await;
        assert_eq!(
            text, f.texts[i],
            "all chapter text and offsets stay unchanged"
        );
    }
    f.s.stop().await;
}

#[tokio::test]
async fn omitting_the_option_preserves_whole_book_generation() {
    let (f, b) = free().await;
    let job = f.make_ready(json!({ "kind": "whole_book" })).await;
    assert_eq!(job["chapters_total"], 4);
    f.wait("jobs", job["id"].as_str().unwrap()).await;
    assert_eq!(f.states().await, ["ready", "ready", "ready", "ready"]);
    assert_eq!(b.spoken().len(), 4);
    f.s.stop().await;
}

#[tokio::test]
async fn empty_free_scope_completes_without_contacting_the_provider() {
    let (f, b) = free().await;
    let job = f
        .make_ready(json!({
            "kind": "from_chapter", "from_chapter_id": f.chapters[3], "include_matter": false,
        }))
        .await;
    assert_eq!(job["chapters_total"], 0);
    f.wait("jobs", job["id"].as_str().unwrap()).await;
    assert!(b.spoken().is_empty());
    assert_eq!(b.state.lock().unwrap().received, 0);
    f.s.stop().await;
}

#[tokio::test]
async fn play_ahead_skips_matter_but_explicit_matter_requests_are_honored() {
    let b = FakeBreeze::start().await;
    let f = Fx::with_chapters(
        TestServer::start().await,
        "breeze",
        json!({ "base_url": b.url }),
        "Mara",
        vec![
            ("Chapter One", "Mira found the first lantern."),
            ("Dedication", "For every imaginary ferrymaker."),
            ("Chapter Two", "Mira found the second lantern."),
            ("Chapter Three", "Mira found the third lantern."),
            (
                "Acknowledgements",
                "Thanks to the imaginary lanternkeepers.",
            ),
        ],
        &["story", "front_matter", "story", "story", "back_matter"],
    )
    .await;
    let request = |i: usize| {
        format!(
            "/api/audiobooks/{}/chapters/{}/request",
            f.audiobook, f.chapters[i]
        )
    };
    let job =
        f.s.post(
            REQUEST,
            &request(0),
            json!({ "ahead": 2, "include_matter": false }),
            202,
        )
        .await;
    assert_eq!(job["chapters_total"], 3);
    f.wait("jobs", job["id"].as_str().unwrap()).await;
    assert_eq!(
        f.states().await,
        ["ready", "not_yet", "ready", "ready", "not_yet"]
    );
    let spoken = b.spoken();
    assert_eq!(spoken.len(), 3);
    for (speech, ordinal) in spoken.iter().zip(["first", "second", "third"]) {
        assert!(speech.contains(ordinal));
    }
    let job =
        f.s.post(
            REQUEST,
            &request(1),
            json!({ "ahead": 0, "include_matter": false }),
            202,
        )
        .await;
    assert_eq!(job["chapters_total"], 1);
    f.wait("jobs", job["id"].as_str().unwrap()).await;
    assert_eq!(
        f.states().await,
        ["ready", "ready", "ready", "ready", "not_yet"]
    );
    assert!(b.spoken()[3].contains("imaginary ferrymaker"));
    assert!(!b.spoken().join("\n").contains("lanternkeepers"));
    f.s.stop().await;
}

#[tokio::test]
async fn premium_previews_use_the_same_story_selection_for_every_scope() {
    let (f, g) = premium().await;
    let cases = [
        (
            json!({ "kind": "whole_book", "include_matter": false }),
            vec![1, 2],
        ),
        (
            json!({ "kind": "from_chapter", "from_chapter_id": f.chapters[0], "include_matter": false }),
            vec![1, 2],
        ),
        (
            json!({ "kind": "from_chapter", "from_chapter_id": f.chapters[2], "include_matter": false }),
            vec![2],
        ),
        (
            json!({ "kind": "chapters", "chapter_ids": [f.chapters[3], f.chapters[2], f.chapters[0], f.chapters[1], f.chapters[1]], "include_matter": false }),
            vec![1, 2],
        ),
    ];
    for (scope, selected) in cases {
        let preview = f.preview(scope, 200).await;
        assert_eq!(preview["scope"]["include_matter"], false);
        assert_eq!(preview["chapters_to_make"], selected.len());
        assert_eq!(preview["text_characters"], f.characters(&selected));
        assert_eq!(preview["chapters_reused"], 0);
    }
    let all = f.preview(json!({ "kind": "whole_book" }), 200).await;
    assert_eq!(all["scope"]["include_matter"], true);
    assert_eq!(all["chapters_to_make"], 4);
    for scope in [
        json!({ "kind": "from_chapter", "from_chapter_id": "unknown", "include_matter": false }),
        json!({ "kind": "chapters", "chapter_ids": [f.chapters[0], "unknown"], "include_matter": false }),
    ] {
        assert_eq!(f.preview(scope, 404).await["code"], "chapter_not_found");
    }
    let empty = f
        .preview(
            json!({ "kind": "chapters", "chapter_ids": [f.chapters[0], f.chapters[3]], "include_matter": false }),
            200,
        )
        .await;
    assert_eq!(empty["chapters_to_make"], 0);
    assert_eq!(empty["text_characters"], 0);
    assert_eq!(empty["cost"]["likely"], money(0));
    assert_eq!(f.approve(&empty, 409).await["code"], "nothing_to_make");
    assert_eq!(
        g.received(),
        0,
        "previews and an empty approval never spend"
    );
    f.s.stop().await;
}

#[tokio::test]
async fn approved_premium_scope_stays_filtered_and_cannot_spend_on_matter() {
    let (f, g) = premium().await;
    let preview = f
        .preview(
            json!({ "kind": "whole_book", "include_matter": false }),
            200,
        )
        .await;
    let plan = f.approve(&preview, 201).await;
    assert_eq!(plan["scope"]["include_matter"], false);
    assert_eq!(plan["chapters_total"], 2);
    for i in [0, 3] {
        let denied =
            f.s.post(
                REQUEST,
                &format!(
                    "/api/audiobooks/{}/chapters/{}/request",
                    f.audiobook, f.chapters[i]
                ),
                json!({ "ahead": 0 }),
                409,
            )
            .await;
        assert_eq!(denied["code"], "plan_required");
    }
    let done = f.wait("plans", plan["id"].as_str().unwrap()).await;
    assert_eq!(done["scope"]["include_matter"], false);
    assert_eq!(done["chapters_done"], 2);
    assert_eq!(f.states().await, ["not_yet", "ready", "ready", "not_yet"]);
    let spoken = g.spoken();
    assert_eq!(spoken.len(), 2);
    assert!(spoken[0].contains("🏮") && spoken[1].contains("Mira rowed"));

    let story = f
        .preview(
            json!({ "kind": "whole_book", "include_matter": false }),
            200,
        )
        .await;
    assert_eq!(story["chapters_to_make"], 0);
    assert_eq!(story["chapters_reused"], 2);
    assert_eq!(story["text_characters"], 0);
    let all = f.preview(json!({ "kind": "whole_book" }), 200).await;
    assert_eq!(all["chapters_to_make"], 2);
    assert_eq!(all["chapters_reused"], 2);
    assert_eq!(all["text_characters"], f.characters(&[0, 3]));
    assert_eq!(
        g.received(),
        2,
        "excluded matter remains outside the paid gate"
    );
    f.s.stop().await;
}
