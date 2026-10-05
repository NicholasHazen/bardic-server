//! One server-owned sample request per voice revision. HTTP cancellation drops a waiter,
//! not the provider request or its spending settlement. No store lock lives in this gate.

use crate::error::ApiError;
use futures_util::FutureExt;
use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

type Key = (String, String);
type Outcome = Option<Result<PathBuf, ApiError>>;

#[derive(Default)]
struct State {
    closed: bool,
    flights: HashMap<Key, watch::Receiver<Outcome>>,
}

#[derive(Default)]
pub struct SampleFlights {
    state: Mutex<State>,
}

impl SampleFlights {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn run<F, Fut>(
        self: &Arc<Self>,
        voice: String,
        revision: String,
        work: F,
    ) -> Result<PathBuf, ApiError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<PathBuf, ApiError>> + Send + 'static,
    {
        let key = (voice, revision);
        let mut rx = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return Err(stopping());
            }
            if let Some(rx) = state.flights.get(&key) {
                rx.clone()
            } else {
                let (tx, rx) = watch::channel(None);
                state.flights.insert(key.clone(), rx.clone());
                let flights = self.clone();
                tokio::spawn(async move {
                    let result = AssertUnwindSafe(async move { work().await })
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| {
                            Err(ApiError::internal("voice sample worker panicked"))
                        });
                    // No filesystem or database work happens after publishing completion: shutdown may return.
                    let mut state = flights.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.flights.remove(&key);
                    tx.send_replace(Some(result));
                });
                rx
            }
        };
        wait(&mut rx).await
    }

    /// Seal new work and wait for all writers before the instance lock can be released.
    pub async fn shutdown(&self) {
        let flights = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            state.flights.values().cloned().collect::<Vec<_>>()
        };
        for mut rx in flights {
            let _ = wait(&mut rx).await;
        }
    }
}

fn stopping() -> ApiError {
    ApiError::conflict(
        "source_unreachable",
        "Bardic is stopping. Try again after it starts.",
    )
}

async fn wait(rx: &mut watch::Receiver<Outcome>) -> Result<PathBuf, ApiError> {
    loop {
        if let Some(result) = rx.borrow().clone() {
            return result;
        }
        rx.changed()
            .await
            .map_err(|_| ApiError::internal("voice sample worker stopped"))?;
    }
}

/// Breeze's streaming read timeout alone can be extended indefinitely by keep-alive events.
/// Bound the whole free sample; unlike a premium request, stopping it cannot orphan spending.
pub(crate) async fn bounded_free_sample<T>(
    duration: std::time::Duration,
    work: impl Future<Output = T>,
) -> Result<T, ApiError> {
    tokio::time::timeout(duration, work).await.map_err(|_| {
        ApiError::conflict(
            "source_unreachable",
            "The voice server did not finish the sample. Try again.",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn panics_release_the_flight_and_a_retry_can_finish() {
        let flights = Arc::new(SampleFlights::new());
        let result = flights
            .run("v".into(), "r".into(), || async {
                panic!("synthetic worker panic")
            })
            .await;
        assert_eq!(result.unwrap_err().code, "internal_error");
        let result = flights
            .run("v".into(), "r".into(), || async {
                Ok(PathBuf::from("cached"))
            })
            .await;
        assert_eq!(result.unwrap(), PathBuf::from("cached"));
    }

    #[tokio::test]
    async fn a_stalled_free_stream_times_out_releases_its_flight_and_can_retry() {
        let flights = Arc::new(SampleFlights::new());
        let result = flights
            .run("v".into(), "r".into(), || async {
                bounded_free_sample(
                    std::time::Duration::from_millis(10),
                    std::future::pending::<Result<PathBuf, ApiError>>(),
                )
                .await?
            })
            .await;
        assert_eq!(result.unwrap_err().code, "source_unreachable");
        let result = flights
            .run("v".into(), "r".into(), || async {
                Ok(PathBuf::from("ready"))
            })
            .await;
        assert_eq!(result.unwrap(), PathBuf::from("ready"));
        flights.shutdown().await;
    }

    #[tokio::test]
    async fn cancelling_the_only_waiter_keeps_work_alive_and_shutdown_drains_it() {
        let flights = Arc::new(SampleFlights::new());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let completed = Arc::new(AtomicUsize::new(0));
        let worker = {
            let (f, e, r, c) = (
                flights.clone(),
                entered.clone(),
                release.clone(),
                completed.clone(),
            );
            tokio::spawn(async move {
                f.run("v".into(), "r".into(), || async move {
                    e.notify_one();
                    r.notified().await;
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(PathBuf::from("kept"))
                })
                .await
            })
        };
        entered.notified().await;
        worker.abort();
        let drain = {
            let f = flights.clone();
            tokio::spawn(async move { f.shutdown().await })
        };
        tokio::task::yield_now().await;
        assert!(!drain.is_finished());
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        release.notify_one();
        drain.await.unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(
            flights
                .run("other".into(), "r".into(), || async { Ok(PathBuf::new()) })
                .await
                .unwrap_err()
                .code,
            "source_unreachable"
        );
    }
}
