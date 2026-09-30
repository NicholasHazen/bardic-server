//! Pure pieces of making audio: splitting a chapter into requests, turning a
//! voice server's sentence timings into per-line timings, and WAV framing.
//! No database, no network.

pub const SAMPLE_RATE: u32 = 24_000;
const BYTES_PER_SECOND: i64 = SAMPLE_RATE as i64 * 2;

/// A line of a chapter: its code point span.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub id: String,
    pub start: i64,
    pub end: i64,
}

/// A sentence the voice server timed, relative to the start of its request.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub char_start: i64,
    pub char_end: i64,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Group consecutive lines into requests of at most `max_chars` characters of
/// chapter text (a single longer line goes alone). A request covers the exact
/// chapter text from its first line's start to its last line's end.
pub fn chunk_lines(lines: &[Line], max_chars: i64) -> Vec<std::ops::Range<usize>> {
    let mut out = vec![];
    let mut first = 0;
    for i in 0..lines.len() {
        if i > first && lines[i].end - lines[first].start > max_chars {
            out.push(first..i);
            first = i;
        }
    }
    if first < lines.len() {
        out.push(first..lines.len());
    }
    out
}

/// Duration in milliseconds of 16-bit mono PCM.
pub fn pcm_ms(bytes: usize) -> i64 {
    bytes as i64 * 1000 / BYTES_PER_SECOND
}

/// Whether the server's segments are usable: in order, inside the text and the audio.
fn segments_ok(segments: &[Segment], text_len: i64, chunk_ms: i64) -> bool {
    let mut cursor = 0;
    !segments.is_empty()
        && segments.iter().all(|s| {
            let ok = 0 <= s.char_start
                && s.char_start < s.char_end
                && s.char_end <= text_len
                && cursor <= s.start_ms
                && s.start_ms <= s.end_ms
                && s.end_ms <= chunk_ms + 50;
            cursor = s.end_ms;
            ok
        })
}

/// Start and end time (ms, relative to the request's audio) of each line.
/// `lines` are in chapter coordinates and `chunk_start` is where the request's
/// text begins in the chapter. Uses the server's segments when they are
/// trustworthy, otherwise spreads the audio over the lines by length.
pub fn line_times(
    lines: &[Line],
    chunk_start: i64,
    chunk_ms: i64,
    segments: &[Segment],
) -> Vec<(i64, i64)> {
    let text_len = lines.last().map_or(0, |l| l.end) - chunk_start;
    let mut times: Vec<(i64, i64)> = if segments_ok(segments, text_len, chunk_ms) {
        let mut prev_end = 0;
        lines
            .iter()
            .map(|l| {
                let (ls, le) = (l.start - chunk_start, l.end - chunk_start);
                let hit: Vec<&Segment> = segments
                    .iter()
                    .filter(|s| s.char_start < le && s.char_end > ls)
                    .collect();
                let t = match (hit.first(), hit.last()) {
                    (Some(a), Some(b)) => (a.start_ms, b.end_ms),
                    _ => (prev_end, prev_end),
                };
                prev_end = t.1;
                t
            })
            .collect()
    } else {
        let total: i64 = lines.iter().map(|l| l.end - l.start).sum::<i64>().max(1);
        let mut done = 0;
        lines
            .iter()
            .map(|l| {
                let start = chunk_ms * done / total;
                done += l.end - l.start;
                (start, chunk_ms * done / total)
            })
            .collect()
    };
    let mut floor = 0;
    for t in &mut times {
        t.0 = t.0.clamp(floor, chunk_ms);
        t.1 = t.1.clamp(t.0, chunk_ms);
        floor = t.0;
    }
    times
}

/// The 44-byte header of a 16-bit mono PCM WAV of `data_len` bytes.
pub fn wav_header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(spans: &[(i64, i64)]) -> Vec<Line> {
        spans
            .iter()
            .enumerate()
            .map(|(i, (s, e))| Line {
                id: format!("l{i}"),
                start: *s,
                end: *e,
            })
            .collect()
    }

    #[test]
    fn chunks_hold_whole_lines_and_a_long_line_goes_alone() {
        let l = lines(&[(0, 100), (102, 300), (302, 900), (902, 950)]);
        assert_eq!(chunk_lines(&l, 350), vec![0..2, 2..3, 3..4]);
        assert_eq!(chunk_lines(&l, 10_000), vec![0..4]);
        assert_eq!(chunk_lines(&l, 50), vec![0..1, 1..2, 2..3, 3..4]);
        assert!(chunk_lines(&[], 100).is_empty());
    }

    #[test]
    fn segments_give_each_line_its_spoken_span() {
        let l = lines(&[(10, 30), (32, 60)]);
        let seg = |a, b, s, e| Segment {
            char_start: a,
            char_end: b,
            start_ms: s,
            end_ms: e,
        };
        // text starts at 10: line 0 is 0..20, line 1 is 22..50
        let segs = [
            seg(0, 10, 0, 800),
            seg(10, 20, 800, 1500),
            seg(22, 50, 1900, 4000),
        ];
        assert_eq!(
            line_times(&l, 10, 4000, &segs),
            vec![(0, 1500), (1900, 4000)]
        );
    }

    #[test]
    fn untrustworthy_segments_fall_back_to_length() {
        let l = lines(&[(0, 10), (12, 40)]);
        let bad = [Segment {
            char_start: 0,
            char_end: 999,
            start_ms: 0,
            end_ms: 10,
        }];
        let t = line_times(&l, 0, 4000, &bad);
        assert_eq!(t[0].0, 0);
        assert_eq!(t[1].1, 4000);
        assert!(t[0].1 <= t[1].0 + 1 && t[0].1 < t[1].1);
        assert_eq!(line_times(&l, 0, 4000, &[]), t);
    }

    #[test]
    fn times_never_run_backwards_or_past_the_audio() {
        let l = lines(&[(0, 10), (10, 20)]);
        let segs = [Segment {
            char_start: 0,
            char_end: 20,
            start_ms: 0,
            end_ms: 4040,
        }];
        for (s, e) in line_times(&l, 0, 4000, &segs) {
            assert!(s <= e && e <= 4000);
        }
    }

    #[test]
    fn wav_header_describes_the_data() {
        let h = wav_header(48_000);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 36 + 48_000);
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 24_000);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 48_000);
        assert_eq!(pcm_ms(48_000), 1000);
    }
}
