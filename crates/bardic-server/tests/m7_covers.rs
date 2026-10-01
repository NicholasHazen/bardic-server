//! M7: chapter counts (all vs story) and generated covers. Every response is validated against the contract.
mod common;
use bardic_server::cover::{generated_hue, sha256_hex};
use chrono::Duration;
use common::{epub::Epub, TestServer};
use serde_json::{json, Value};
use std::time::Duration as StdDuration;

const B: &str = "/api/books";
const BID: &str = "/api/books/{book_id}";
const COVER: &str = "/api/books/{book_id}/cover";
const REFRESH: &str = "/api/books/{book_id}/cover/refresh";

async fn server() -> TestServer {
    let s = TestServer::start().await;
    let l = s.listener("Nick").await;
    s.act_as(&l);
    s
}

fn txt(chapters: &[(&str, &str)]) -> Vec<u8> {
    chapters
        .iter()
        .map(|(h, b)| format!("{h}\n\n{b}\n\n"))
        .collect::<String>()
        .into_bytes()
}

async fn book(s: &TestServer, id: &str) -> Value {
    s.get(BID, &format!("{B}/{id}"), 200).await
}

async fn fetch_cover(s: &TestServer, cover: &Value) -> (reqwest::header::HeaderMap, Vec<u8>) {
    let r = s
        .client
        .get(format!("{}{}", s.base, cover["url"].as_str().unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let headers = r.headers().clone();
    (headers, r.bytes().await.unwrap().to_vec())
}

fn assert_generated(cover: &Value) {
    assert_eq!(cover["generated"], true, "{cover}");
    assert_eq!(cover["width"], 240);
    assert_eq!(cover["height"], 360);
    assert_eq!(cover["sample"]["vivid"], true, "{cover}");
}

#[tokio::test]
async fn chapter_count_counts_everything_and_story_chapter_count_only_the_story() {
    let s = server().await;
    let e = Epub {
        chapters: vec![
            ("Copyright", "All rights reserved."),
            ("The Crossing", "The ferry left at dusk."),
            ("The Far Shore", "Nobody was there."),
            ("Acknowledgements", "Thanks to the ferrymen."),
        ],
        ..Epub::default()
    };
    let id = s.add_book("matter.epub", e.build()).await;
    let b = book(&s, &id).await;
    assert_eq!(b["chapter_count"], 4);
    assert_eq!(b["story_chapter_count"], 2);
    let chapters = s
        .get(
            "/api/books/{book_id}/chapters",
            &format!("{B}/{id}/chapters"),
            200,
        )
        .await;
    assert_eq!(chapters["items"].as_array().unwrap().len(), 4);

    // The same numbers on every place a Book appears.
    let list = s.get(B, B, 200).await;
    let item = &list["items"][0];
    assert_eq!(item["chapter_count"], 4);
    assert_eq!(item["story_chapter_count"], 2);
    let edited = s
        .patch(BID, &format!("{B}/{id}"), json!({ "author": "Z" }), 200)
        .await;
    assert_eq!(edited["chapter_count"], 4);
    assert_eq!(edited["story_chapter_count"], 2);
    let removed = s
        .post(
            "/api/books/{book_id}/remove",
            &format!("{B}/{id}/remove"),
            json!({}),
            200,
        )
        .await;
    assert_eq!(removed["chapter_count"], 4);
    assert_eq!(removed["story_chapter_count"], 2);
    s.stop().await;
}

#[tokio::test]
async fn plain_text_has_no_matter_so_both_counts_are_equal() {
    let s = server().await;
    let file = txt(&[
        ("Chapter 1", "The river was high."),
        ("Chapter 2", "It fell by morning."),
        ("Chapter 3", "Then it rose."),
    ]);
    let id = s.add_book("river.txt", file).await;
    let b = book(&s, &id).await;
    assert_eq!(b["chapter_count"], 3);
    assert_eq!(b["story_chapter_count"], 3);

    let sample = s
        .post("/api/books/sample", "/api/books/sample", json!({}), 201)
        .await;
    assert_eq!(sample["chapter_count"], 3);
    assert_eq!(sample["story_chapter_count"], 3);
    s.stop().await;
}

#[tokio::test]
async fn text_imports_the_sample_and_coverless_epubs_get_a_generated_cover() {
    let s = server().await;
    let sample = s
        .post("/api/books/sample", "/api/books/sample", json!({}), 201)
        .await;
    let text_id = s
        .add_book(
            "river days.txt",
            txt(&[("Chapter 1", "The river was high.")]),
        )
        .await;
    let e = Epub {
        cover: None,
        ..Epub::default()
    };
    let epub_id = s.add_book("nocover.epub", e.build()).await;

    for b in [sample, book(&s, &text_id).await, book(&s, &epub_id).await] {
        let cover = &b["cover"];
        assert_generated(cover);
        let (h, bytes) = fetch_cover(&s, cover).await;
        assert_eq!(h["content-type"], "image/jpeg");
        let cc = h["cache-control"].to_str().unwrap();
        assert!(
            cc.contains("immutable") && cc.contains("max-age=31536000"),
            "{cc}"
        );
        assert_eq!(
            h["etag"].to_str().unwrap().trim_matches('"'),
            sha256_hex(&bytes)
        );
        assert_eq!(cover["sha256"], sha256_hex(&bytes).as_str());
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (240, 360));
        // The sample is measured from the picture and its hue is the one derived from the title.
        let want = generated_hue(b["title"].as_str().unwrap(), b["author"].as_str().unwrap());
        let gap = (cover["sample"]["hue"].as_f64().unwrap() - want).rem_euclid(360.0);
        assert!(
            gap.min(360.0 - gap) < 60.0,
            "hue {} wanted {want}",
            cover["sample"]["hue"]
        );
    }
    s.stop().await;
}

#[tokio::test]
async fn a_generated_cover_is_the_same_for_the_same_title_and_author_and_differs_otherwise() {
    let s = server().await;
    // Different text, same title (from the file name) and author: the same cover on every import.
    let a = s
        .add_book("shared title.txt", txt(&[("Chapter 1", "One.")]))
        .await;
    let b = s
        .add_book(
            "shared title.txt",
            txt(&[("Chapter 1", "Two, quite different.")]),
        )
        .await;
    let c = s
        .add_book("another title.txt", txt(&[("Chapter 1", "One.")]))
        .await;
    let (a, b, c) = (book(&s, &a).await, book(&s, &b).await, book(&s, &c).await);
    assert_ne!(a["id"], b["id"]);
    assert_eq!(a["cover"]["sha256"], b["cover"]["sha256"]);
    assert_ne!(a["cover"]["sha256"], c["cover"]["sha256"]);
    s.stop().await;
}

#[tokio::test]
async fn a_real_epub_cover_stays_real_and_refresh_keeps_it() {
    let s = server().await;
    let id = s.add_book("x.epub", Epub::default().build()).await;
    let b = book(&s, &id).await;
    assert_eq!(b["cover"]["generated"], false);
    let again = s
        .post(REFRESH, &format!("{B}/{id}/cover/refresh"), json!({}), 200)
        .await;
    assert_eq!(again["cover"]["generated"], false);
    assert_eq!(again["cover"]["sha256"], b["cover"]["sha256"]);
    // Editing the title does not touch a real cover.
    s.patch(BID, &format!("{B}/{id}"), json!({ "title": "Other" }), 200)
        .await;
    let again = s
        .post(REFRESH, &format!("{B}/{id}/cover/refresh"), json!({}), 200)
        .await;
    assert_eq!(again["cover"]["sha256"], b["cover"]["sha256"]);
    assert_eq!(again["cover"]["generated"], false);
    s.stop().await;
}

#[tokio::test]
async fn refreshing_a_generated_cover_follows_the_current_title_and_author() {
    let s = server().await;
    let id = s
        .add_book("old name.txt", txt(&[("Chapter 1", "Words.")]))
        .await;
    let before = book(&s, &id).await;
    let same = s
        .post(REFRESH, &format!("{B}/{id}/cover/refresh"), json!({}), 200)
        .await;
    assert_eq!(same["cover"]["sha256"], before["cover"]["sha256"]);

    // Editing alone leaves the cover; refreshing redraws it.
    let edited = s
        .patch(
            BID,
            &format!("{B}/{id}"),
            json!({ "title": "A New Name" }),
            200,
        )
        .await;
    assert_eq!(edited["cover"]["sha256"], before["cover"]["sha256"]);
    let after = s
        .post(REFRESH, &format!("{B}/{id}/cover/refresh"), json!({}), 200)
        .await;
    assert_ne!(after["cover"]["sha256"], before["cover"]["sha256"]);
    assert_generated(&after["cover"]);
    let fresh = s
        .add_book("a new name.txt", txt(&[("Chapter 1", "x")]))
        .await;
    assert_eq!(
        book(&s, &fresh).await["cover"]["sha256"],
        after["cover"]["sha256"],
        "same title, same colour"
    );
    // The new image is what is served.
    let (_, bytes) = fetch_cover(&s, &after["cover"]).await;
    assert_eq!(
        sha256_hex(&bytes),
        after["cover"]["sha256"].as_str().unwrap()
    );
    s.stop().await;
}

#[tokio::test]
async fn books_that_had_no_cover_get_one_at_start_up() {
    let s = TestServer::start().await;
    let l = s.listener("Nick").await;
    s.act_as(&l);
    let id = s
        .add_book("old book.txt", txt(&[("Chapter 1", "Words.")]))
        .await;
    let removed = s
        .add_book("shelved.txt", txt(&[("Chapter 1", "Words.")]))
        .await;
    s.post(
        "/api/books/{book_id}/remove",
        &format!("{B}/{removed}/remove"),
        json!({}),
        200,
    )
    .await;
    let want = book(&s, &id).await["cover"]["sha256"].clone();
    let real = s.add_book("x.epub", Epub::default().build()).await;
    let real_sha = book(&s, &real).await["cover"]["sha256"].clone();
    let dir = s.stop().await;

    // A database from before generated covers: no cover on the text books.
    {
        let conn = rusqlite::Connection::open(dir.path().join("bardic.db")).unwrap();
        let n = conn
            .execute(
                "UPDATE books SET cover_sha256=NULL,cover_width=NULL,cover_height=NULL,cover_jpeg=NULL,cover_sample=NULL,cover_generated=0 WHERE source_name LIKE '%.txt'",
                [],
            )
            .unwrap();
        assert_eq!(n, 2);
    }

    let s = TestServer::start_in(dir).await;
    s.act_as(&l);
    let b = book(&s, &id).await;
    assert_generated(&b["cover"]);
    assert_eq!(
        b["cover"]["sha256"], want,
        "the same cover it would have had at import"
    );
    let (h, _) = fetch_cover(&s, &b["cover"]).await;
    assert_eq!(h["content-type"], "image/jpeg");
    assert_generated(&book(&s, &removed).await["cover"]);
    // A real cover is untouched.
    let r = book(&s, &real).await;
    assert_eq!(r["cover"]["generated"], false);
    assert_eq!(r["cover"]["sha256"], real_sha);

    // Starting again changes nothing.
    let dir = s.stop().await;
    let s = TestServer::start_in(dir).await;
    s.act_as(&l);
    assert_eq!(book(&s, &id).await["cover"]["sha256"], want);
    s.stop().await;
}

#[tokio::test]
async fn the_cover_goes_with_the_book_and_no_cover_file_is_left() {
    let s = server().await;
    let id = s
        .add_book("doomed.txt", txt(&[("Chapter 1", "Words.")]))
        .await;
    assert_generated(&book(&s, &id).await["cover"]);
    let dp = format!("{B}/{id}/deletion");
    s.post("/api/books/{book_id}/deletion", &dp, json!({}), 202)
        .await;
    s.clock.advance(Duration::seconds(61));
    let mut done = Value::Null;
    for _ in 0..80 {
        done = s.get("/api/books/{book_id}/deletion", &dp, 200).await;
        if done["state"] == "done" {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    assert_eq!(done["state"], "done");
    s.get(COVER, &format!("{B}/{id}/cover"), 404).await;

    // Covers live in the database row (not in files), so the row going is the cleanup.
    let conn = rusqlite::Connection::open(s.dir.path().join("bardic.db")).unwrap();
    let left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM books WHERE cover_jpeg IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0);
    fn files(dir: &std::path::Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                files(&p, out);
            } else {
                out.push(p.to_string_lossy().to_lowercase());
            }
        }
    }
    let mut all = vec![];
    files(s.dir.path(), &mut all);
    assert!(
        !all.iter()
            .any(|f| f.contains("cover") || f.ends_with(".jpg") || f.ends_with(".jpeg")),
        "{all:?}"
    );
    s.stop().await;
}
