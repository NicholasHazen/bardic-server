//! M2: places, history, finished, and the per-listener parts of the library.
//! Every response is validated against the contract.
mod common;
use chrono::Duration;
use common::{TestServer, DEVICE};
use reqwest::Method;
use serde_json::{json, Value};

const OTHER_DEVICE: &str = "test-device-0002";
const PLACE: &str = "/api/books/{book_id}/place";

struct Fx {
    s: TestServer,
    book: String,
    chapters: Vec<String>,
    lens: Vec<i64>,
}

impl Fx {
    async fn new() -> Self {
        let s = TestServer::start().await;
        let l = s.listener("Nick").await;
        s.act_as(&l);
        let b = s
            .post("/api/books/sample", "/api/books/sample", json!({}), 201)
            .await;
        let book = b["id"].as_str().unwrap().to_string();
        let ch = s
            .get(
                "/api/books/{book_id}/chapters",
                &format!("/api/books/{book}/chapters"),
                200,
            )
            .await;
        let mut chapters = vec![];
        let mut lens = vec![];
        for c in ch["items"].as_array().unwrap() {
            let id = c["id"].as_str().unwrap().to_string();
            let t = s
                .get(
                    "/api/books/{book_id}/chapters/{chapter_id}/text",
                    &format!("/api/books/{book}/chapters/{id}/text"),
                    200,
                )
                .await;
            lens.push(t["text"].as_str().unwrap().chars().count() as i64);
            chapters.push(id);
        }
        Fx {
            s,
            book,
            chapters,
            lens,
        }
    }

    fn path(&self, tail: &str) -> String {
        format!("/api/books/{}/place{tail}", self.book)
    }

    fn input(&self, ch: usize, offset: i64, base: i64) -> Value {
        json!({ "chapter_id": self.chapters[ch], "offset": offset, "mode": "listening", "base_revision": base })
    }

    async fn put_as(&self, device: &str, body: Value, expect: u16) -> Value {
        self.s
            .call(
                Method::PUT,
                PLACE,
                &self.path(""),
                Some(device),
                Some(body),
                expect,
            )
            .await
    }

    async fn put(&self, ch: usize, offset: i64, base: i64) -> Value {
        self.put_as(DEVICE, self.input(ch, offset, base), 200).await
    }

    async fn get(&self) -> Value {
        self.s.get(PLACE, &self.path(""), 200).await
    }

    async fn history(&self) -> Vec<Value> {
        let h = self
            .s
            .get(
                "/api/books/{book_id}/place/history",
                &self.path("/history"),
                200,
            )
            .await;
        h["items"].as_array().unwrap().clone()
    }

    async fn finish(&self, finished: bool, expect: u16) -> Value {
        self.s
            .put(
                "/api/books/{book_id}/place/finished",
                &self.path("/finished"),
                json!({ "finished": finished }),
                expect,
            )
            .await
    }
}

#[tokio::test]
async fn a_new_book_has_no_place_and_writing_creates_revision_one() {
    let f = Fx::new().await;
    let e = f.s.get(PLACE, &f.path(""), 404).await;
    assert_eq!(e["code"], "place_not_found");
    assert_eq!(f.history().await.len(), 0);

    let p = f.put(1, 20, 0).await;
    assert_eq!(p["revision"], 1);
    assert_eq!(p["offset"], 20);
    assert_eq!(p["chapter_id"], f.chapters[1]);
    assert_eq!(p["device_id"], DEVICE);
    assert_eq!(p["finished"]["finished"], false);
    assert!(p["progress"].as_f64().unwrap() > 0.0);
    assert_eq!(f.get().await["revision"], 1);

    // the book carries the listener's summary
    let b =
        f.s.get(
            "/api/books/{book_id}",
            &format!("/api/books/{}", f.book),
            200,
        )
        .await;
    assert_eq!(b["place"]["chapter_id"], f.chapters[1]);
    assert_eq!(b["place"]["finished"], false);
    f.s.stop().await;
}

#[tokio::test]
async fn an_identical_write_changes_nothing_and_a_move_bumps_the_revision() {
    let f = Fx::new().await;
    let a = f.put(0, 10, 0).await;
    f.s.clock.advance(Duration::seconds(5));
    let b = f.put(0, 10, 1).await;
    assert_eq!(b["revision"], 1);
    assert_eq!(
        b["updated_at"], a["updated_at"],
        "the clock is not reset by an identical write"
    );
    let c = f.put(0, 11, 1).await;
    assert_eq!(c["revision"], 2);
    f.s.stop().await;
}

#[tokio::test]
async fn invalid_places_are_rejected() {
    let f = Fx::new().await;
    let e = f.put_as(DEVICE, f.input(0, f.lens[0] + 1, 0), 400).await;
    assert_eq!(e["code"], "offset_out_of_range");
    f.put(0, f.lens[0], 0).await; // the very end is allowed
    let e = f
        .put_as(
            DEVICE,
            json!({ "chapter_id": "nope", "offset": 0, "mode": "reading", "base_revision": 0 }),
            404,
        )
        .await;
    assert_eq!(e["code"], "chapter_not_found");
    let e = f.put_as(DEVICE, json!({ "chapter_id": f.chapters[0], "offset": 0, "mode": "flying", "base_revision": 0 }), 400).await;
    assert_eq!(e["code"], "invalid_request");
    let e = f.put_as(DEVICE, json!({ "chapter_id": f.chapters[0], "offset": -1, "mode": "reading", "base_revision": 0 }), 400).await;
    assert_eq!(e["code"], "invalid_request");
    let e =
        f.s.get("/api/books/{book_id}/place", "/api/books/nope/place", 404)
            .await;
    assert_eq!(e["code"], "book_not_found");
    f.s.stop().await;
}

#[tokio::test]
async fn another_device_with_a_stale_revision_gets_the_server_place() {
    let f = Fx::new().await;
    f.put(0, 10, 0).await; // device A, revision 1

    // device B has never seen it
    let e = f.put_as(OTHER_DEVICE, f.input(1, 5, 0), 409).await;
    assert_eq!(e["code"], "place_conflict");
    assert_eq!(e["server_place"]["revision"], 1);
    assert_eq!(e["server_place"]["device_id"], DEVICE);
    assert_eq!(
        f.get().await["chapter_id"],
        f.chapters[0],
        "a conflict changes nothing"
    );

    // it chooses its own place by writing again with the server's revision
    let ok = f.put_as(OTHER_DEVICE, f.input(1, 5, 1), 200).await;
    assert_eq!(ok["revision"], 2);
    assert_eq!(ok["device_id"], OTHER_DEVICE);

    // device A catching up with its own old revision is not a conflict with itself...
    // ...but B wrote last, so A is now the stale one.
    let e = f.put_as(DEVICE, f.input(0, 30, 1), 409).await;
    assert_eq!(e["server_place"]["revision"], 2);

    // a device is never in conflict with its own earlier write
    f.put_as(OTHER_DEVICE, f.input(1, 6, 1), 200).await;
    f.s.stop().await;
}

#[tokio::test]
async fn history_keeps_old_places_and_big_jumps_not_small_steps() {
    let f = Fx::new().await;
    f.put(0, 10, 0).await;
    f.s.clock.advance(Duration::seconds(5));
    f.put(0, 12, 1).await; // small step, recent: the place being replaced is not kept
    assert_eq!(f.history().await.len(), 0);

    f.s.clock.advance(Duration::minutes(31));
    f.put(0, 14, 2).await; // the replaced place was at least 30 minutes old
    let h = f.history().await;
    assert_eq!(h.len(), 1);
    assert_eq!(h[0]["offset"], 12);

    f.s.clock.advance(Duration::seconds(5));
    f.put(2, f.lens[2] / 2, 3).await; // a jump of a few percent, so the replaced place is kept
    let h = f.history().await;
    assert_eq!(h.len(), 2);
    assert_eq!(h[0]["offset"], 14, "newest first");
    assert_eq!(h[0]["finished"]["finished"], false);
    f.s.stop().await;
}

#[tokio::test]
async fn history_is_capped_at_ten() {
    let f = Fx::new().await;
    for (rev, i) in (0..14).enumerate() {
        f.put(0, i, rev as i64).await;
        f.s.clock.advance(Duration::minutes(31));
    }
    let h = f.history().await;
    assert_eq!(h.len(), 10);
    assert_eq!(h[0]["offset"], 12, "the latest replaced place");
    assert_eq!(h[9]["offset"], 3, "the oldest kept one");
    f.s.stop().await;
}

#[tokio::test]
async fn marking_finished_and_reopening() {
    let f = Fx::new().await;
    f.finish(false, 404).await; // nothing to reopen

    let p = f.finish(true, 200).await; // no place yet: one is created at the end
    assert_eq!(p["finished"]["finished"], true);
    assert_eq!(p["finished"]["reason"], "marked");
    assert!(p["finished"]["since"].is_string());
    assert!(p["progress"].as_f64().unwrap() > 0.99);
    assert_eq!(p["revision"], 1);

    // moving the place clears a marked finish
    let q = f.put(0, 3, 1).await;
    assert_eq!(q["finished"]["finished"], false);
    assert_eq!(q["finished"]["reason"], Value::Null);

    // marking keeps the place
    let r = f.finish(true, 200).await;
    assert_eq!(r["chapter_id"], f.chapters[0]);
    assert_eq!(r["offset"], 3);
    assert_eq!(r["finished"]["reason"], "marked");
    assert_eq!(r["revision"], 3);

    let s = f.finish(false, 200).await;
    assert_eq!(s["finished"]["finished"], false);
    assert_eq!(s["offset"], 3);
    f.s.stop().await;
}

#[tokio::test]
async fn reaching_the_end_counts_as_finished_only_after_a_day_unchanged() {
    let f = Fx::new().await;
    let last = f.chapters.len() - 1;
    f.put(last, f.lens[last], 0).await;
    f.s.clock.advance(Duration::hours(23));
    assert_eq!(f.get().await["finished"]["finished"], false);

    f.s.clock.advance(Duration::hours(2));
    let p = f.get().await;
    assert_eq!(p["finished"]["finished"], true);
    assert_eq!(p["finished"]["reason"], "reached_end");

    // any change restarts the clock
    f.put(last, f.lens[last] - 1, 1).await;
    f.s.clock.advance(Duration::hours(23));
    assert_eq!(f.get().await["finished"]["finished"], false);

    // reopening restarts it too
    f.s.clock.advance(Duration::hours(2));
    assert_eq!(f.get().await["finished"]["finished"], true);
    f.finish(false, 200).await;
    assert_eq!(f.get().await["finished"]["finished"], false);
    f.s.stop().await;
}

#[tokio::test]
async fn clearing_removes_the_place_and_its_history_and_is_idempotent() {
    let f = Fx::new().await;
    f.put(0, 10, 0).await;
    f.s.clock.advance(Duration::minutes(31));
    f.put(0, 50, 1).await;
    assert_eq!(f.history().await.len(), 1);

    f.s.delete(PLACE, &f.path(""), 204).await;
    f.s.delete(PLACE, &f.path(""), 204).await;
    f.s.get(PLACE, &f.path(""), 404).await;
    assert_eq!(f.history().await.len(), 0);
    let b =
        f.s.get(
            "/api/books/{book_id}",
            &format!("/api/books/{}", f.book),
            200,
        )
        .await;
    assert_eq!(b["place"], Value::Null);
    f.s.delete(PLACE, "/api/books/nope/place", 404).await;
    f.s.stop().await;
}

#[tokio::test]
async fn places_belong_to_the_listener_and_drive_filters_and_recency() {
    let f = Fx::new().await;
    let nick = f.s.as_listener.lock().unwrap().clone().unwrap();
    let second =
        f.s.post("/api/books/sample", "/api/books/sample", json!({}), 201)
            .await["id"]
            .as_str()
            .unwrap()
            .to_string();
    let ids = |v: &Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap().to_string())
            .collect()
    };
    let list = |q: &str| {
        let s = &f.s;
        let q = q.to_string();
        async move { s.get("/api/books", &format!("/api/books{q}"), 200).await }
    };

    assert_eq!(ids(&list("?filter=not_started").await).len(), 2);
    assert_eq!(ids(&list("?filter=in_progress").await).len(), 0);

    f.put(0, 10, 0).await; // first book in progress
    f.s.clock.advance(Duration::minutes(1));
    // second book: finished
    let ch2 =
        f.s.get(
            "/api/books/{book_id}/chapters",
            &format!("/api/books/{second}/chapters"),
            200,
        )
        .await;
    let _ = ch2;
    f.s.put(
        "/api/books/{book_id}/place/finished",
        &format!("/api/books/{second}/place/finished"),
        json!({ "finished": true }),
        200,
    )
    .await;

    assert_eq!(
        ids(&list("?filter=in_progress").await),
        vec![f.book.clone()]
    );
    assert_eq!(ids(&list("?filter=finished").await), vec![second.clone()]);
    assert_eq!(ids(&list("?filter=not_started").await).len(), 0);
    let recent = ids(&list("?sort=recent").await);
    assert_eq!(recent[0], second, "latest place first");
    let page = list("?filter=all&limit=1").await;
    assert_eq!(ids(&page).len(), 1);
    assert!(page["next"].is_string());

    // another listener sees none of it
    let other = f.s.listener("Sam").await;
    f.s.act_as(&other);
    assert_eq!(ids(&list("?filter=not_started").await).len(), 2);
    f.s.get(PLACE, &f.path(""), 404).await;
    let series = f.s.get("/api/series", "/api/series", 200).await;
    let _ = series;
    let l =
        f.s.get(
            "/api/listeners/{listener_id}",
            &format!("/api/listeners/{nick}"),
            200,
        )
        .await;
    assert_eq!(l["books_started"], 2);
    assert!(l["last_listened_at"].is_string());
    f.s.stop().await;
}

#[tokio::test]
async fn book_writes_show_the_place_when_a_listener_is_named() {
    let f = Fx::new().await;
    f.put(0, 10, 0).await;
    let with =
        f.s.call(
            Method::POST,
            "/api/books/{book_id}/remove",
            &format!("/api/books/{}/remove", f.book),
            Some(DEVICE),
            Some(json!({})),
            200,
        )
        .await;
    assert_eq!(with["place"]["chapter_id"], f.chapters[0]);
    f.s.act_as_nobody();
    let without =
        f.s.call(
            Method::POST,
            "/api/books/{book_id}/restore",
            &format!("/api/books/{}/restore", f.book),
            Some(DEVICE),
            Some(json!({})),
            200,
        )
        .await;
    assert_eq!(without["place"], Value::Null);
    f.s.stop().await;
}

#[tokio::test]
async fn deleting_a_listener_deletes_their_places() {
    let f = Fx::new().await;
    f.put(0, 10, 0).await;
    let l = f.s.as_listener.lock().unwrap().clone().unwrap();
    let keep = f.s.listener("Sam").await; // a listener must remain
    let impact =
        f.s.get(
            "/api/listeners/{listener_id}/impact",
            &format!("/api/listeners/{l}/impact"),
            200,
        )
        .await;
    assert_eq!(impact["places"], 1);
    f.s.delete(
        "/api/listeners/{listener_id}",
        &format!("/api/listeners/{l}"),
        204,
    )
    .await;
    f.s.act_as(&keep);
    f.s.get(PLACE, &f.path(""), 404).await;
    f.s.stop().await;
}
