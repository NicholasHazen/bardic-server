//! M1b: the library. Every response is validated against the contract.
mod common;
use common::{epub::Epub, name_body, TestServer, DEVICE};
use serde_json::{json, Value};

const B: &str = "/api/books";
const BID: &str = "/api/books/{book_id}";

async fn server_with_listener() -> (TestServer, String) {
    let s = TestServer::start().await;
    let l = s.listener("Nick").await;
    s.act_as(&l);
    (s, l)
}

fn txt(chapters: &[(&str, &str)]) -> Vec<u8> {
    chapters
        .iter()
        .map(|(h, b)| format!("{h}\n\n{b}\n\n"))
        .collect::<String>()
        .into_bytes()
}

fn slice(text: &str, start: i64, end: i64) -> String {
    text.chars()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect()
}

#[tokio::test]
async fn listener_scoped_reads_need_a_listener() {
    let s = TestServer::start().await;
    let e = s.get(B, B, 400).await;
    assert_eq!(e["code"], "listener_required");
    s.act_as("01AAAAAAAAAAAAAAAAAAAAAAAA");
    let e = s.get(B, B, 404).await;
    assert_eq!(e["code"], "listener_not_found");
    s.stop().await;
}

#[tokio::test]
async fn the_sample_book_is_readable_at_once_with_exact_lines() {
    let (s, _) = server_with_listener().await;
    let book = s
        .post("/api/books/sample", "/api/books/sample", json!({}), 201)
        .await;
    assert_eq!(book["state"], "readable");
    assert_eq!(book["title"], "The Lantern Keeper");
    assert_eq!(book["chapter_count"], 3);
    assert!(book["word_count"].as_i64().unwrap() > 300);
    assert_eq!(book["cover"], Value::Null);
    assert_eq!(book["place"], Value::Null);
    let id = book["id"].as_str().unwrap();

    let ch = s
        .get(
            "/api/books/{book_id}/chapters",
            &format!("{B}/{id}/chapters"),
            200,
        )
        .await;
    let items = ch["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["index"], 0);
    assert_eq!(items[0]["kind"], "story");

    let cid = items[0]["id"].as_str().unwrap();
    let t = s
        .get(
            "/api/books/{book_id}/chapters/{chapter_id}/text",
            &format!("{B}/{id}/chapters/{cid}/text"),
            200,
        )
        .await;
    let text = t["text"].as_str().unwrap();
    let lines = t["lines"].as_array().unwrap();
    assert!(lines.len() >= 5);
    // Lines tile the paragraphs: each slice is a whole paragraph of the exact text.
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    assert_eq!(lines.len(), paragraphs.len());
    for (l, p) in lines.iter().zip(paragraphs) {
        assert_eq!(
            slice(
                text,
                l["start"].as_i64().unwrap(),
                l["end"].as_i64().unwrap()
            ),
            p
        );
    }
    assert_eq!(t["text_sha256"], items[0]["text_sha256"]);
    assert_eq!(common_sha(text), t["text_sha256"].as_str().unwrap());

    let e = s.get(BID, &format!("{B}/{id}/cover"), 404).await;
    assert_eq!(e["code"], "cover_not_found");
    let e = s
        .get(
            "/api/books/{book_id}/chapters/{chapter_id}/text",
            &format!("{B}/{id}/chapters/nope/text"),
            404,
        )
        .await;
    assert_eq!(e["code"], "chapter_not_found");
    s.stop().await;
}

fn common_sha(text: &str) -> String {
    bardic_server::cover::sha256_hex(text.as_bytes())
}

#[tokio::test]
async fn a_text_file_becomes_a_book_with_chapters_and_its_original_is_kept() {
    let (s, _) = server_with_listener().await;
    let file = txt(&[
        ("Chapter 1", "The river was high."),
        ("Chapter 2", "It fell by morning."),
    ]);
    let expect_sha = bardic_server::cover::sha256_hex(&file);
    let (st, imp) = s.upload("river_days.txt", file, None).await;
    assert_eq!(st, 202);
    assert!(matches!(
        imp["state"].as_str(),
        Some("queued" | "reading" | "finding_chapters" | "preparing_text" | "done")
    ));
    let done = s.wait_import(imp["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "done");
    assert_eq!(done["progress"], 1.0);
    assert_eq!(done["error"], Value::Null);
    let id = done["book_id"].as_str().unwrap();

    let book = s.get(BID, &format!("{B}/{id}"), 200).await;
    assert_eq!(book["title"], "river days");
    assert_eq!(book["author"], "");
    assert_eq!(book["chapter_count"], 2);
    assert_eq!(book["source_sha256"], expect_sha.as_str());
    let original = s.dir.path().join("originals").join(id).join("source.txt");
    assert!(original.exists(), "original kept at {}", original.display());
    let tmp_files = std::fs::read_dir(s.dir.path().join("tmp"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(tmp_files, 0, "uploads are not left in tmp");
    s.stop().await;
}

#[tokio::test]
async fn an_epub_brings_title_author_cover_and_a_colour_sample() {
    let (s, _) = server_with_listener().await;
    let id = s.add_book("x.epub", Epub::default().build()).await;
    let book = s.get(BID, &format!("{B}/{id}"), 200).await;
    assert_eq!(book["title"], "The Test Ferry");
    assert_eq!(book["author"], "A. Writer");
    let cover = &book["cover"];
    let sample = &cover["sample"];
    assert_eq!(sample["vivid"], true);
    let hue = sample["hue"].as_f64().unwrap();
    assert!(!(12.0..=348.0).contains(&hue), "red cover, got hue {hue}");
    assert!(cover["url"]
        .as_str()
        .unwrap()
        .contains(cover["sha256"].as_str().unwrap()));

    // The cover is served immutably.
    let r = s
        .client
        .get(format!("{}{}", s.base, cover["url"].as_str().unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/jpeg");
    assert!(r.headers()["cache-control"]
        .to_str()
        .unwrap()
        .contains("immutable"));
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    let bytes = r.bytes().await.unwrap();
    assert_eq!(
        etag.trim_matches('"'),
        bardic_server::cover::sha256_hex(&bytes)
    );
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "JPEG");

    // Refresh re-reads it from the saved original.
    let again = s
        .post(
            "/api/books/{book_id}/cover/refresh",
            &format!("{B}/{id}/cover/refresh"),
            json!({}),
            200,
        )
        .await;
    assert_eq!(
        again["cover"]["sha256"], cover["sha256"],
        "same file, same thumbnail"
    );
    s.stop().await;
}

#[tokio::test]
async fn files_that_cannot_be_added_fail_clearly_and_leave_nothing_behind() {
    let (s, _) = server_with_listener().await;
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "protected.epub",
            Epub {
                drm: true,
                ..Default::default()
            }
            .build(),
            "import_drm_protected",
        ),
        (
            "binary.txt",
            vec![b'a', 0, b'b'],
            "import_unsupported_encoding",
        ),
        (
            "bad.txt",
            vec![0x66, 0xff, 0xfe],
            "import_unsupported_encoding",
        ),
        ("empty.txt", b"   \n\n ".to_vec(), "import_no_text"),
        (
            "broken.epub",
            b"PK-not-an-epub".to_vec(),
            "import_unreadable",
        ),
    ];
    for (name, bytes, code) in cases {
        let (st, imp) = s.upload(name, bytes, None).await;
        assert_eq!(st, 202, "{name}");
        let done = s.wait_import(imp["id"].as_str().unwrap()).await;
        assert_eq!(done["state"], "failed", "{name}: {done}");
        assert_eq!(done["error"]["code"], code, "{name}");
        assert_eq!(done["book_id"], Value::Null, "{name}");
    }
    // No book, no original, no leftover upload.
    assert_eq!(
        s.get(B, &format!("{B}?include_removed=true"), 200).await["items"],
        json!([])
    );
    let originals = std::fs::read_dir(s.dir.path().join("originals"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(originals, 0);
    let tmp = std::fs::read_dir(s.dir.path().join("tmp"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(tmp, 0);
    s.stop().await;
}

#[tokio::test]
async fn bad_uploads_are_refused_before_any_work() {
    let (s, _) = server_with_listener().await;
    // Wrong type
    let (st, e) = s.upload("book.pdf", b"%PDF".to_vec(), None).await;
    assert_eq!(
        (st, e["code"].as_str().unwrap()),
        (400, "import_unreadable")
    );
    // No file field
    let r = s
        .client
        .post(format!("{}/api/imports", s.base))
        .header("x-bardic-device", DEVICE)
        .multipart(reqwest::multipart::Form::new().text("other", "x"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    s.stop().await;

    // Over the limit
    let dir = tempfile::tempdir().unwrap();
    let s = TestServer::start_with(dir, |c| c.max_upload_bytes = 2_000).await;
    let (st, e) = s.upload("big.txt", vec![b'a'; 10_000], None).await;
    assert_eq!((st, e["code"].as_str().unwrap()), (413, "import_too_large"));
    s.stop().await;
}

#[tokio::test]
async fn importing_twice_with_one_key_adds_one_book() {
    let (s, _) = server_with_listener().await;
    let file = txt(&[("Chapter 1", "One."), ("Chapter 2", "Two.")]);
    let (_, a) = s.upload("a.txt", file.clone(), Some("key-123")).await;
    let (st, b) = s.upload("a.txt", file, Some("key-123")).await;
    assert_eq!(st, 202);
    assert_eq!(a["id"], b["id"]);
    s.wait_import(a["id"].as_str().unwrap()).await;
    assert_eq!(s.get(B, B, 200).await["items"].as_array().unwrap().len(), 1);
    s.stop().await;
}

#[tokio::test]
async fn cancelling_leaves_no_book_and_finishing_first_is_fine() {
    let (s, _) = server_with_listener().await;
    let big = "Chapter 1\n\n".to_string() + &"A sentence that repeats many times. ".repeat(400_000);
    let (_, imp) = s.upload("big.txt", big.into_bytes(), None).await;
    let id = imp["id"].as_str().unwrap().to_string();
    s.delete(
        "/api/imports/{import_id}",
        &format!("/api/imports/{id}"),
        204,
    )
    .await;
    let done = s.wait_import(&id).await;
    match done["state"].as_str().unwrap() {
        "cancelled" => {
            assert_eq!(done["book_id"], Value::Null);
            assert_eq!(
                s.get(B, B, 200).await["items"],
                json!([]),
                "a cancelled import leaves no book"
            );
        }
        "done" => {} // it finished before the cancel landed
        other => panic!("unexpected state {other}"),
    }
    // Cancelling again, or after it finished, changes nothing.
    s.delete(
        "/api/imports/{import_id}",
        &format!("/api/imports/{id}"),
        204,
    )
    .await;
    let e = s
        .delete("/api/imports/{import_id}", "/api/imports/nope", 404)
        .await;
    assert_eq!(e["code"], "import_not_found");
    s.stop().await;
}

#[tokio::test]
async fn an_unfinished_import_is_cleaned_up_when_the_server_restarts() {
    let s = TestServer::start().await;
    let dir = s.stop().await;
    {
        let conn = rusqlite::Connection::open(dir.path().join("bardic.db")).unwrap();
        conn.execute("INSERT INTO books(id,title,author,state,added_at) VALUES('b1','Half','','adding','2026-01-01T00:00:00.000Z')", []).unwrap();
        conn.execute("INSERT INTO imports(id,state,file_name,created_at,progress,book_id) VALUES('i1','reading','half.txt','2026-01-01T00:00:00.000Z',0.1,'b1')", []).unwrap();
    }
    let s = TestServer::start_in(dir).await;
    let l = s.listener("Nick").await;
    s.act_as(&l);
    let imp = s
        .get("/api/imports/{import_id}", "/api/imports/i1", 200)
        .await;
    assert_eq!(imp["state"], "failed");
    assert_eq!(imp["error"]["code"], "import_interrupted");
    assert_eq!(s.get(B, B, 200).await["items"], json!([]));
    s.stop().await;
}

#[tokio::test]
async fn chapter_text_cannot_be_changed_even_by_mistake() {
    let (s, _) = server_with_listener().await;
    s.add_book(
        "a.txt",
        txt(&[
            ("Chapter 1", "Original words."),
            ("Chapter 2", "More words."),
        ]),
    )
    .await;
    let conn = rusqlite::Connection::open(s.dir.path().join("bardic.db")).unwrap();
    let err = conn
        .execute("UPDATE chapters SET text='changed'", [])
        .unwrap_err();
    assert!(err.to_string().contains("immutable"), "{err}");
    let err = conn
        .execute("UPDATE chapters SET text_sha256='x'", [])
        .unwrap_err();
    assert!(err.to_string().contains("immutable"), "{err}");
    drop(conn);
    s.stop().await;
}

#[tokio::test]
async fn duplicates_are_reported_but_never_blocked() {
    let (s, _) = server_with_listener().await;
    let file = Epub::default().build();
    let sha = bardic_server::cover::sha256_hex(&file);
    let d = "/api/books/duplicates";
    let none = s.get(d, &format!("{d}?sha256={sha}"), 200).await;
    assert_eq!(none, json!({ "exact": [], "similar": [] }));

    let a = s.add_book("a.epub", file.clone()).await;
    let b = s.add_book("b.epub", file.clone()).await; // not blocked
    let found = s.get(d, &format!("{d}?sha256={sha}"), 200).await;
    let ids: Vec<&str> = found["exact"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["book_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));

    // A removed book still counts, so the client can offer Restore.
    s.post(
        "/api/books/{book_id}/remove",
        &format!("{B}/{a}/remove"),
        json!({}),
        200,
    )
    .await;
    let found = s.get(d, &format!("{d}?sha256={sha}"), 200).await;
    assert!(found["exact"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["state"] == "removed"));

    // Same title and author, different bytes: similar.
    let other = Epub {
        chapters: vec![("Only", "Different words entirely.")],
        ..Default::default()
    }
    .build();
    let osha = bardic_server::cover::sha256_hex(&other);
    let sim = s
        .get(
            d,
            &format!("{d}?sha256={osha}&title=the%20TEST%20ferry!&author=a.%20writer"),
            200,
        )
        .await;
    assert_eq!(sim["exact"], json!([]));
    assert_eq!(sim["similar"].as_array().unwrap().len(), 2);

    let e = s.get(d, &format!("{d}?sha256=ABC"), 400).await;
    assert_eq!(e["code"], "invalid_request");
    s.stop().await;
}

#[tokio::test]
async fn listing_searching_sorting_and_paging() {
    let (s, _) = server_with_listener().await;
    for (name, title, author) in [
        ("a", "Zebra Crossing", "Ann"),
        ("b", "apple Orchard", "Zed"),
        ("c", "Mango Lane", "Bea"),
    ] {
        let id = s
            .add_book(
                &format!("{name}.epub"),
                Epub {
                    title: Box::leak(title.to_string().into_boxed_str()),
                    author: Box::leak(author.to_string().into_boxed_str()),
                    ..Default::default()
                }
                .build(),
            )
            .await;
        assert!(!id.is_empty());
    }
    let titles = |v: &Value| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["title"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        titles(&s.get(B, &format!("{B}?sort=title"), 200).await),
        ["apple Orchard", "Mango Lane", "Zebra Crossing"]
    );
    assert_eq!(
        titles(&s.get(B, &format!("{B}?sort=author"), 200).await),
        ["Zebra Crossing", "Mango Lane", "apple Orchard"]
    );
    assert_eq!(
        titles(&s.get(B, &format!("{B}?sort=added"), 200).await),
        ["Mango Lane", "apple Orchard", "Zebra Crossing"]
    );
    assert_eq!(
        titles(&s.get(B, &format!("{B}?q=ORCHARD"), 200).await),
        ["apple Orchard"]
    );
    assert_eq!(
        titles(&s.get(B, &format!("{B}?q=bea"), 200).await),
        ["Mango Lane"],
        "matches author"
    );
    assert_eq!(
        titles(&s.get(B, &format!("{B}?q=100%25"), 200).await),
        Vec::<String>::new(),
        "% is literal"
    );

    let p1 = s.get(B, &format!("{B}?sort=title&limit=2"), 200).await;
    assert_eq!(titles(&p1).len(), 2);
    let next = p1["next"].as_str().unwrap().to_string();
    let p2 = s
        .get(B, &format!("{B}?sort=title&limit=2&after={next}"), 200)
        .await;
    assert_eq!(titles(&p2), ["Zebra Crossing"]);
    assert_eq!(p2["next"], Value::Null);

    // Nobody has a place yet: in_progress and finished are empty, not_started is everything.
    assert_eq!(
        titles(&s.get(B, &format!("{B}?filter=in_progress"), 200).await).len(),
        0
    );
    assert_eq!(
        titles(&s.get(B, &format!("{B}?filter=finished"), 200).await).len(),
        0
    );
    assert_eq!(
        titles(&s.get(B, &format!("{B}?filter=not_started"), 200).await).len(),
        3
    );

    for bad in [
        "filter=nope",
        "sort=nope",
        "limit=0",
        "after=zzz",
        "limit=x",
    ] {
        assert_eq!(
            s.get(B, &format!("{B}?{bad}"), 400).await["code"],
            "invalid_request",
            "{bad}"
        );
    }
    s.stop().await;
}

#[tokio::test]
async fn editing_removing_and_restoring() {
    let (s, _) = server_with_listener().await;
    let id = s.add_book("a.epub", Epub::default().build()).await;
    let p = format!("{B}/{id}");
    let b = s.patch(BID, &p, json!({ "title": "  New Title ", "author": "New Author", "series": { "name": "The Cycle", "order": 2 } }), 200).await;
    assert_eq!(
        (b["title"].as_str(), b["author"].as_str()),
        (Some("New Title"), Some("New Author"))
    );
    assert_eq!(b["series"], json!({ "name": "The Cycle", "order": 2.0 }));
    // Absent leaves alone; null clears.
    let b = s.patch(BID, &p, json!({ "title": "Again" }), 200).await;
    assert_eq!(b["series"]["name"], "The Cycle");
    let b = s.patch(BID, &p, json!({ "series": null }), 200).await;
    assert_eq!(b["series"], Value::Null);
    // Validation
    for bad in [
        json!({ "title": "  " }),
        json!({ "title": "x".repeat(201) }),
        json!({ "series": { "name": "" } }),
    ] {
        assert_eq!(s.patch(BID, &p, bad, 400).await["code"], "invalid_request");
    }
    assert_eq!(
        s.patch(BID, &format!("{B}/nope"), json!({ "title": "x" }), 404)
            .await["code"],
        "book_not_found"
    );

    // Text was never touched.
    let ch = s
        .get(
            "/api/books/{book_id}/chapters",
            &format!("{p}/chapters"),
            200,
        )
        .await;
    assert_eq!(ch["items"].as_array().unwrap().len(), 2);

    // Remove: hidden from the library, idempotent, cannot be edited, restorable.
    let r = s
        .post(
            "/api/books/{book_id}/remove",
            &format!("{p}/remove"),
            json!({}),
            200,
        )
        .await;
    assert_eq!(r["state"], "removed");
    s.post(
        "/api/books/{book_id}/remove",
        &format!("{p}/remove"),
        json!({}),
        200,
    )
    .await;
    assert_eq!(s.get(B, B, 200).await["items"], json!([]));
    assert_eq!(
        s.get(B, &format!("{B}?include_removed=true"), 200).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        s.patch(BID, &p, json!({ "title": "x" }), 409).await["code"],
        "book_removed"
    );
    // Its text can still be read.
    s.get(
        "/api/books/{book_id}/chapters",
        &format!("{p}/chapters"),
        200,
    )
    .await;
    let r = s
        .post(
            "/api/books/{book_id}/restore",
            &format!("{p}/restore"),
            json!({}),
            200,
        )
        .await;
    assert_eq!(r["state"], "readable");
    assert_eq!(s.get(B, B, 200).await["items"].as_array().unwrap().len(), 1);

    let audit = s.get("/api/audit", "/api/audit?limit=100", 200).await;
    let actions: Vec<&str> = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["action"].as_str().unwrap())
        .collect();
    for a in [
        "book.imported",
        "book.updated",
        "book.removed",
        "book.restored",
    ] {
        assert!(actions.contains(&a), "{a} in {actions:?}");
    }
    s.stop().await;
}

#[tokio::test]
async fn series_group_books_and_name_missing_volumes() {
    let (s, _) = server_with_listener().await;
    let mut ids = Vec::new();
    for (i, order) in [1.0, 2.0, 4.0].iter().enumerate() {
        let id = s
            .add_book(
                &format!("{i}.epub"),
                Epub {
                    title: Box::leak(format!("Vol {order}").into_boxed_str()),
                    ..Default::default()
                }
                .build(),
            )
            .await;
        s.patch(
            BID,
            &format!("{B}/{id}"),
            json!({ "series": { "name": "The Cycle", "order": order } }),
            200,
        )
        .await;
        ids.push(id);
    }
    let lone = s.add_book("z.epub", Epub::default().build()).await;
    s.patch(
        BID,
        &format!("{B}/{lone}"),
        json!({ "series": { "name": "Halves", "order": 1.5 } }),
        200,
    )
    .await;
    let out = s.get("/api/series", "/api/series", 200).await;
    let items = out["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let cycle = items.iter().find(|x| x["name"] == "The Cycle").unwrap();
    assert_eq!(cycle["missing_orders"], json!([3]));
    let order: Vec<f64> = cycle["books"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["series"]["order"].as_f64().unwrap())
        .collect();
    assert_eq!(order, [1.0, 2.0, 4.0]);
    let halves = items.iter().find(|x| x["name"] == "Halves").unwrap();
    assert_eq!(
        halves["missing_orders"],
        json!([]),
        "fractional orders name no gaps"
    );
    // Removed books leave the series.
    s.post(
        "/api/books/{book_id}/remove",
        &format!("{B}/{}/remove", ids[1]),
        json!({}),
        200,
    )
    .await;
    let out = s.get("/api/series", "/api/series", 200).await;
    let cycle = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "The Cycle")
        .unwrap()
        .clone();
    assert_eq!(cycle["missing_orders"], json!([2, 3]));
    s.stop().await;
}

#[tokio::test]
async fn search_is_case_insensitive_and_offsets_point_at_the_match() {
    let (s, _) = server_with_listener().await;
    let id = s
        .add_book(
            "s.txt",
            txt(&[
                (
                    "Chapter 1",
                    "The Ferryman waited. \"You are early,\" the ferryman said.",
                ),
                (
                    "Chapter 2",
                    "Caf\u{00E9} \u{1F600} ferryMAN again, \u{00C9}COLE.",
                ),
            ]),
        )
        .await;
    let q = |extra: &str| format!("{B}/{id}/search?{extra}");
    let t = "/api/books/{book_id}/search";
    let r = s.get(t, &q("q=FERRYMAN"), 200).await;
    assert_eq!(r["total"], 3);
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    // Each hit's offsets slice the exact chapter text to the matched words.
    for hit in items {
        let cid = hit["chapter_id"].as_str().unwrap();
        let text = s
            .get(
                "/api/books/{book_id}/chapters/{chapter_id}/text",
                &format!("{B}/{id}/chapters/{cid}/text"),
                200,
            )
            .await;
        let got = slice(
            text["text"].as_str().unwrap(),
            hit["start"].as_i64().unwrap(),
            hit["end"].as_i64().unwrap(),
        );
        assert_eq!(got.to_lowercase(), "ferryman");
        assert_eq!(got, hit["match"].as_str().unwrap());
        assert!(text["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["id"] == hit["line_id"]));
    }
    // Accented letters fold, and offsets count code points after an emoji.
    let r = s.get(t, &q("q=%C3%A9cole"), 200).await;
    assert_eq!(r["total"], 1);
    // Paging
    let p1 = s.get(t, &q("q=ferryman&limit=2"), 200).await;
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    let next = p1["next"].as_str().unwrap();
    let p2 = s
        .get(t, &q(&format!("q=ferryman&limit=2&after={next}")), 200)
        .await;
    assert_eq!(p2["items"].as_array().unwrap().len(), 1);
    assert_eq!(p2["next"], Value::Null);
    assert_eq!(s.get(t, &q("q=zzzz"), 200).await["total"], 0);
    assert_eq!(s.get(t, &q("q="), 400).await["code"], "invalid_request");
    // a search term has a size limit: it is scanned against every chapter
    s.get(t, &q(&format!("q={}", "a".repeat(200))), 200).await;
    assert_eq!(
        s.get(t, &q(&format!("q={}", "a".repeat(201))), 400).await["code"],
        "invalid_request"
    );
    assert_eq!(
        s.get(t, &format!("{B}/nope/search?q=a"), 404).await["code"],
        "book_not_found"
    );
    s.stop().await;
}

#[tokio::test]
async fn library_changes_are_announced() {
    let (s, _) = server_with_listener().await;
    let mut resp = s
        .client
        .get(format!("{}/api/events", s.base))
        .header("x-bardic-device", DEVICE)
        .send()
        .await
        .unwrap();
    let id = s
        .add_book(
            "a.txt",
            txt(&[("Chapter 1", "One."), ("Chapter 2", "Two.")]),
        )
        .await;
    let mut seen = String::new();
    let ok = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(chunk) = resp.chunk().await.unwrap() {
            seen.push_str(&String::from_utf8_lossy(&chunk));
            if seen.contains("import.updated")
                && seen.contains("book.updated")
                && seen.contains(&id)
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(ok, "missing notices; saw {seen}");
    drop(resp);
    let _ = name_body;
    s.stop().await;
}

#[tokio::test]
async fn an_epub_that_lists_the_same_chapter_many_times_is_read_once() {
    let (s, _) = server_with_listener().await;
    let id = s
        .add_book(
            "loop.epub",
            Epub {
                spine_repeats: 500,
                ..Default::default()
            }
            .build(),
        )
        .await;
    let ch = s
        .get(
            "/api/books/{book_id}/chapters",
            &format!("/api/books/{id}/chapters"),
            200,
        )
        .await;
    assert_eq!(ch["items"].as_array().unwrap().len(), 2, "{ch}");
    s.stop().await;
}
