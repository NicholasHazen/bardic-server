//! Performance at 500 books (spec section 10.1). Not run by default; use release mode:
//!
//!   cargo test -p bardic-server --release --test perf -- --ignored --nocapture
//!
//! Books are original synthetic text (about 300 KB, 24 chapters each). Timings are taken with a
//! plain client, so the contract check does not count.
mod common;
use common::{TestServer, DEVICE};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const BOOKS: usize = 500;
const CHAPTERS: usize = 24;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn book_text(n: usize) -> String {
    const SYL: [&str; 24] = [
        "lan", "tern", "mor", "wen", "dar", "ik", "sol", "eth", "bra", "vin", "cor", "ul", "pe",
        "ran", "tho", "mis", "gal", "or", "fen", "yl", "ash", "red", "mar", "it",
    ];
    let mut r = Rng(0x9E37_79B9_7F4A_7C15 ^ ((n as u64 + 1) * 0x1234_5678_9ABC_DEF1));
    let mut out = format!("Synthetic Volume {n}\n\n");
    for c in 1..=CHAPTERS {
        out.push_str(&format!("Chapter {c}\n\n"));
        let mut bytes = 0;
        while bytes < 12_000 {
            let mut para = String::new();
            for s in 0..(3 + r.next() % 4) {
                if s > 0 {
                    para.push(' ');
                }
                let words = 8 + r.next() % 10;
                for w in 0..words {
                    if w > 0 {
                        para.push(' ');
                    }
                    for _ in 0..(1 + r.next() % 3) {
                        para.push_str(SYL[(r.next() % 24) as usize]);
                    }
                }
                para.push('.');
            }
            bytes += para.len() + 2;
            out.push_str(&para);
            out.push_str("\n\n");
        }
    }
    out.push_str("The very last line holds the zebracorn marker.\n");
    out
}

struct Timing(Vec<Duration>);
impl Timing {
    fn report(&mut self, what: &str) -> Duration {
        self.0.sort();
        let p = |q: f64| self.0[((self.0.len() - 1) as f64 * q) as usize];
        eprintln!(
            "{what:<44} n={:<3} p50 {:>7.1} ms  p95 {:>7.1} ms  max {:>7.1} ms",
            self.0.len(),
            p(0.5).as_secs_f64() * 1e3,
            p(0.95).as_secs_f64() * 1e3,
            self.0.last().unwrap().as_secs_f64() * 1e3
        );
        p(0.95)
    }
}

async fn time(
    s: &TestServer,
    listener: &str,
    path: &str,
    n: usize,
    method: reqwest::Method,
    body: Option<Value>,
) -> Timing {
    let mut t = vec![];
    for _ in 0..n {
        let mut req = s
            .client
            .request(method.clone(), format!("{}{}", s.base, path))
            .header("x-bardic-device", DEVICE)
            .header("x-bardic-listener", listener);
        if let Some(b) = &body {
            req = req.json(b);
        }
        let t0 = Instant::now();
        let resp = req.send().await.unwrap();
        let st = resp.status();
        let _ = resp.bytes().await.unwrap();
        t.push(t0.elapsed());
        assert!(st.is_success(), "{path}: {st}");
    }
    Timing(t)
}

#[tokio::test]
#[ignore = "benchmark: run with --release"]
async fn five_hundred_books_meet_the_spec_targets() {
    let s = TestServer::start().await;
    let l = s.listener("Perf").await;
    s.act_as(&l);

    let t0 = Instant::now();
    let mut ids = vec![];
    let mut bytes = 0;
    for n in 0..BOOKS {
        let text = book_text(n);
        bytes += text.len();
        ids.push(s.add_book(&format!("vol{n}.txt"), text.into_bytes()).await);
    }
    eprintln!(
        "imported {BOOKS} books, {:.0} MB of text, in {:.1}s ({:.0} ms per book)",
        bytes as f64 / 1e6,
        t0.elapsed().as_secs_f64(),
        t0.elapsed().as_secs_f64() * 1e3 / BOOKS as f64
    );
    let dbsize = std::fs::metadata(s.dir.path().join("bardic.db"))
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!("database {:.0} MB", dbsize as f64 / 1e6);

    // places for a fifth of the library, so the place filters have work to do
    for id in ids.iter().step_by(5) {
        let chapters = s
            .get(
                "/api/books/{book_id}/chapters",
                &format!("/api/books/{id}/chapters"),
                200,
            )
            .await;
        let ch = chapters["items"][3]["id"].as_str().unwrap();
        s.call(
            reqwest::Method::PUT,
            "/api/books/{book_id}/place",
            &format!("/api/books/{id}/place"),
            Some(DEVICE),
            Some(json!({ "chapter_id": ch, "offset": 10, "mode": "reading", "base_revision": 0 })),
            200,
        )
        .await;
    }

    let m = reqwest::Method::GET;
    // the fast answers must be real ones
    let first = |path: String| {
        let (s, l) = (&s, l.clone());
        async move {
            let r: Value = s
                .client
                .get(format!("{}{}", s.base, path))
                .header("x-bardic-device", DEVICE)
                .header("x-bardic-listener", l)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            r
        }
    };
    let page = first("/api/books?limit=50".into()).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 50);
    eprintln!("first title: {}", page["items"][0]["title"]);
    let found = first("/api/books?limit=50&q=vol4".into()).await;
    eprintln!(
        "q=vol4 matched {} books",
        found["items"].as_array().unwrap().len()
    );
    assert!(!found["items"].as_array().unwrap().is_empty());
    let hit = first(format!(
        "/api/books/{}/search?q=zebracorn&limit=20",
        ids[BOOKS / 2]
    ))
    .await;
    assert_eq!(hit["items"].as_array().unwrap().len(), 1, "{hit}");
    let common = first(format!(
        "/api/books/{}/search?q=lantern&limit=20",
        ids[BOOKS / 2]
    ))
    .await;
    eprintln!(
        "'lantern' matches (first page): {}",
        common["items"].as_array().unwrap().len()
    );
    assert!(!common["items"].as_array().unwrap().is_empty());
    let mut worst_list = Duration::ZERO;
    for (what, path) in [
        ("listBooks recent, limit 50", "/api/books?limit=50"),
        ("listBooks sort=title", "/api/books?limit=50&sort=title"),
        (
            "listBooks filter=in_progress",
            "/api/books?limit=50&filter=in_progress",
        ),
        (
            "listBooks filter=not_started",
            "/api/books?limit=50&filter=not_started",
        ),
        ("listBooks q=vol4", "/api/books?limit=50&q=vol4"),
    ] {
        worst_list = worst_list.max(time(&s, &l, path, 20, m.clone(), None).await.report(what));
    }
    // the last page of the whole library, by offset cursor
    let mut after = None;
    let mut pages = 0;
    let t = Instant::now();
    loop {
        let path = match &after {
            None => "/api/books?limit=50&sort=title".to_string(),
            Some(a) => format!("/api/books?limit=50&sort=title&after={a}"),
        };
        let v = time(&s, &l, &path, 1, m.clone(), None).await;
        let _ = v;
        let r: Value = s
            .client
            .get(format!("{}{}", s.base, path))
            .header("x-bardic-device", DEVICE)
            .header("x-bardic-listener", &l)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        pages += 1;
        match r["next"].as_str() {
            Some(n) => after = Some(n.to_string()),
            None => break,
        }
    }
    eprintln!(
        "walk the whole library: {pages} pages in {:.0} ms",
        t.elapsed().as_secs_f64() * 1e3
    );

    let mid = &ids[BOOKS / 2];
    let mut worst_search = Duration::ZERO;
    for (what, q) in [
        ("searchBook: common word", "lantern"),
        ("searchBook: rare phrase at the end", "zebracorn"),
        ("searchBook: no match", "qqqqzzzz"),
    ] {
        let p = format!("/api/books/{mid}/search?q={q}&limit=20");
        worst_search = worst_search.max(time(&s, &l, &p, 20, m.clone(), None).await.report(what));
    }
    let chapters = s
        .get(
            "/api/books/{book_id}/chapters",
            &format!("/api/books/{mid}/chapters"),
            200,
        )
        .await;
    let ch = chapters["items"][10]["id"].as_str().unwrap().to_string();
    time(
        &s,
        &l,
        &format!("/api/books/{mid}/chapters/{ch}/text"),
        20,
        m.clone(),
        None,
    )
    .await
    .report("getChapterText (12 KB chapter)");
    let resume = time(
        &s,
        &l,
        &format!("/api/books/{}/place", ids[0]),
        20,
        m.clone(),
        None,
    )
    .await
    .report("getPlace (resume)");
    time(&s, &l, "/api/server", 20, m.clone(), None)
        .await
        .report("getServer");

    assert!(
        worst_list < Duration::from_secs(1),
        "library takes {worst_list:?}"
    );
    assert!(
        worst_search < Duration::from_millis(500),
        "search takes {worst_search:?}"
    );
    assert!(resume < Duration::from_secs(2));
    s.stop().await;
}
