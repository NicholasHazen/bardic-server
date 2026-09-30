//! The rules for places. Pure: no database, no clock of its own.
//!
//! A *place* is `(chapter, offset)` for one listener in one book, anchored to
//! the text, so it is valid for every voice. These rules are the product spec's
//! C2 and C6: revisions, history, and when a book counts as finished.

use chrono::{DateTime, Duration, Utc};

/// A book is finished automatically at this share of the story text...
pub const FINISH_PROGRESS: f64 = 0.98;
/// ...once the place has been unchanged for this long.
pub const FINISH_AFTER: Duration = Duration::hours(24);
/// Earlier places kept per listener and book.
pub const HISTORY_MAX: i64 = 10;
/// A previous place is kept when it was at least this old...
pub const HISTORY_MIN_AGE: Duration = Duration::minutes(30);
/// ...or when the new place is at least this far away, as a share of the book.
pub const HISTORY_JUMP: f64 = 0.02;

/// Story chapters' text lengths, in reading order, with the kind of each chapter.
pub struct ChapterLen {
    pub kind_is_story: bool,
    pub kind_is_back: bool,
    pub len: i64,
}

/// Share of the story text before `(chapter index, offset)`: 0 in front matter,
/// 1 in back matter, and 0 for a book with no story text.
pub fn progress(chapters: &[ChapterLen], idx: usize, offset: i64) -> f64 {
    let total: i64 = chapters
        .iter()
        .filter(|c| c.kind_is_story)
        .map(|c| c.len)
        .sum();
    let Some(ch) = chapters.get(idx) else {
        return 0.0;
    };
    if total <= 0 {
        return 0.0;
    }
    if !ch.kind_is_story {
        return if ch.kind_is_back { 1.0 } else { 0.0 };
    }
    let before: i64 = chapters[..idx]
        .iter()
        .filter(|c| c.kind_is_story)
        .map(|c| c.len)
        .sum();
    ((before + offset.clamp(0, ch.len)) as f64 / total as f64).clamp(0.0, 1.0)
}

#[derive(Debug, PartialEq)]
pub struct Finished {
    pub finished: bool,
    pub since: Option<DateTime<Utc>>,
    pub reason: Option<&'static str>,
}

/// Evaluated when read, so there is no background job and no clock to drift.
/// Marked finish wins; otherwise progress at least 98% unchanged for over 24 hours.
pub fn finished_state(
    marked_at: Option<DateTime<Utc>>,
    progress: f64,
    updated_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Finished {
    if let Some(m) = marked_at {
        return Finished {
            finished: true,
            since: Some(m),
            reason: Some("marked"),
        };
    }
    if progress >= FINISH_PROGRESS && now - updated_at > FINISH_AFTER {
        return Finished {
            finished: true,
            since: Some(updated_at + FINISH_AFTER),
            reason: Some("reached_end"),
        };
    }
    Finished {
        finished: false,
        since: None,
        reason: None,
    }
}

/// Whether the place being replaced should be kept in history.
pub fn keep_in_history(
    previous_updated_at: DateTime<Utc>,
    now: DateTime<Utc>,
    previous_progress: f64,
    new_progress: f64,
) -> bool {
    now - previous_updated_at >= HISTORY_MIN_AGE
        || (new_progress - previous_progress).abs() >= HISTORY_JUMP - 1e-9
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    fn ch(story: bool, back: bool, len: i64) -> ChapterLen {
        ChapterLen {
            kind_is_story: story,
            kind_is_back: back,
            len,
        }
    }

    #[test]
    fn progress_counts_only_story_text() {
        let cs = [
            ch(false, false, 500),
            ch(true, false, 100),
            ch(true, false, 300),
            ch(false, true, 200),
        ];
        assert_eq!(progress(&cs, 1, 0), 0.0);
        assert_eq!(progress(&cs, 1, 50), 0.125);
        assert_eq!(progress(&cs, 2, 0), 0.25);
        assert_eq!(progress(&cs, 2, 300), 1.0);
        assert_eq!(
            progress(&cs, 0, 499),
            0.0,
            "front matter is before the book"
        );
        assert_eq!(progress(&cs, 3, 10), 1.0, "back matter is after it");
        assert_eq!(progress(&cs, 2, 9999), 1.0, "clamped");
        assert_eq!(
            progress(&[ch(false, false, 10)], 0, 5),
            0.0,
            "no story text"
        );
    }

    #[test]
    fn marked_wins_and_auto_needs_98_percent_for_over_a_day() {
        let start = t("2026-01-01T00:00:00Z");
        let m = finished_state(Some(start), 0.1, start, start);
        assert_eq!((m.finished, m.reason), (true, Some("marked")));
        // Not at 98%: never auto-finished, however long.
        assert!(!finished_state(None, 0.97, start, start + Duration::days(30)).finished);
        // At 98%: not before or at 24 hours, finished after.
        assert!(!finished_state(None, 0.98, start, start + Duration::hours(24)).finished);
        let f = finished_state(
            None,
            0.98,
            start,
            start + Duration::hours(24) + Duration::seconds(1),
        );
        assert_eq!((f.finished, f.reason), (true, Some("reached_end")));
        assert_eq!(f.since, Some(start + Duration::hours(24)));
    }

    #[test]
    fn history_keeps_old_places_and_jumps_but_not_small_steps() {
        let t0 = t("2026-01-01T12:00:00Z");
        assert!(!keep_in_history(t0, t0 + Duration::minutes(29), 0.10, 0.11));
        assert!(
            keep_in_history(t0, t0 + Duration::minutes(30), 0.10, 0.11),
            "age"
        );
        assert!(
            keep_in_history(t0, t0 + Duration::seconds(5), 0.10, 0.12),
            "a 2% jump"
        );
        assert!(
            keep_in_history(t0, t0 + Duration::seconds(5), 0.50, 0.40),
            "backwards too"
        );
        assert!(!keep_in_history(
            t0,
            t0 + Duration::seconds(5),
            0.10,
            0.1199
        ));
    }
}
