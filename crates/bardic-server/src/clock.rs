use chrono::{DateTime, Duration, SecondsFormat, Utc};
use std::sync::Mutex;

/// Source of time. Rules that depend on time (the finished clock, price age,
/// plan waits) take a `Clock` so tests can move it.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock tests control.
pub struct FakeClock(Mutex<DateTime<Utc>>);

impl FakeClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        FakeClock(Mutex::new(start))
    }
    pub fn advance(&self, by: Duration) {
        let mut t = self.0.lock().expect("clock lock");
        *t += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock lock")
    }
}

/// RFC 3339 UTC with millisecond precision. Stored and sent in this one format,
/// so string order equals time order.
pub fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}
