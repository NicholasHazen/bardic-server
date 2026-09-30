use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};
use tokio::sync::broadcast;

/// A change notice: ids only. Clients re-read the resource (see contract `Notice`).
#[derive(Debug, Clone, Serialize)]
pub struct Notice {
    #[serde(rename = "type")]
    pub kind: String,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listener_id: Option<String>,
}

impl Notice {
    pub fn new(kind: &str, at: String) -> Self {
        Notice {
            kind: kind.to_string(),
            at,
            book_id: None,
            id: None,
            listener_id: None,
        }
    }
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}

const RING: usize = 256;

pub struct EventBus {
    tx: broadcast::Sender<(u64, Notice)>,
    next: AtomicU64,
    ring: Mutex<VecDeque<(u64, Notice)>>,
}

/// What a new subscriber starts with.
pub struct Subscription {
    pub backlog: Vec<(u64, Notice)>,
    /// Set when the requested position cannot be replayed: the client must reload.
    pub resync: bool,
    pub rx: broadcast::Receiver<(u64, Notice)>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        EventBus {
            tx,
            next: AtomicU64::new(1),
            ring: Mutex::new(VecDeque::new()),
        }
    }

    pub fn publish(&self, notice: Notice) -> u64 {
        let mut ring = self.ring.lock().expect("event ring");
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        ring.push_back((id, notice.clone()));
        if ring.len() > RING {
            ring.pop_front();
        }
        let _ = self.tx.send((id, notice));
        id
    }

    /// Subscribe, optionally resuming after `last_event_id`.
    pub fn subscribe(&self, last_event_id: Option<u64>) -> Subscription {
        let ring = self.ring.lock().expect("event ring");
        let rx = self.tx.subscribe();
        let newest = self.next.load(Ordering::SeqCst) - 1;
        let (backlog, resync) = match last_event_id {
            None => (Vec::new(), false),
            Some(last) if last > newest => (Vec::new(), true),
            Some(last) => {
                let oldest = ring.front().map(|(id, _)| *id).unwrap_or(newest + 1);
                if last + 1 < oldest {
                    (Vec::new(), true)
                } else {
                    (
                        ring.iter().filter(|(id, _)| *id > last).cloned().collect(),
                        false,
                    )
                }
            }
        };
        Subscription {
            backlog,
            resync,
            rx,
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
