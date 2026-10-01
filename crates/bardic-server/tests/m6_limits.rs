//! M6: limits on work a client can start.
mod common;
use common::{TestServer, DEVICE};

#[tokio::test]
async fn a_burst_of_uploads_is_worked_through_and_all_finish() {
    let s = TestServer::start().await;
    let l = s.listener("Burst").await;
    s.act_as(&l);
    let s = std::sync::Arc::new(s);
    let mut tasks = vec![];
    for i in 0..12 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let text = format!("Chapter One\n\nBook number {i} begins here, with its own words.\n");
            s.add_book(&format!("burst{i}.txt"), text.into_bytes())
                .await
        }));
    }
    let mut ids = std::collections::HashSet::new();
    for t in tasks {
        ids.insert(t.await.unwrap());
    }
    assert_eq!(ids.len(), 12);
    let list = s.get("/api/books", "/api/books?limit=50", 200).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 12);
    std::sync::Arc::into_inner(s).unwrap().stop().await;
}

#[tokio::test]
async fn the_sixty_fifth_event_stream_is_refused_until_one_closes() {
    let s = TestServer::start().await;
    let mut open = vec![];
    for _ in 0..64 {
        let r = s
            .client
            .get(format!("{}/api/events", s.base))
            .header("x-bardic-device", DEVICE)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        open.push(r);
    }
    let refused = s
        .client
        .get(format!("{}/api/events", s.base))
        .header("x-bardic-device", DEVICE)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 429);
    let body: serde_json::Value = refused.json().await.unwrap();
    assert_eq!(body["code"], "too_many_streams");
    // the contract documents it
    s.contract
        .check("GET", "/api/events", 429, Some(&body))
        .expect("429 is documented for streamEvents");
    // closing one makes room
    drop(open.pop());
    let mut ok = false;
    for _ in 0..50 {
        let r = s
            .client
            .get(format!("{}/api/events", s.base))
            .header("x-bardic-device", DEVICE)
            .send()
            .await
            .unwrap();
        if r.status() == 200 {
            ok = true;
            open.push(r);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ok, "a closed stream did not free its place");
    drop(open);
    s.stop().await;
}
