//! Turning book markup into canonical text and lines.
//!
//! Canonical chapter text is paragraphs joined by a blank line. A *line* is the
//! smallest spoken and highlighted unit: one paragraph, or a run of sentences
//! when a paragraph is long. Offsets are zero-based Unicode code points (Rust
//! `char`), end exclusive, never bytes or UTF-16 units.

/// Longest paragraph kept as a single line, in characters.
const MAX_LINE: usize = 600;
/// Shortest chunk a long paragraph is cut into at a sentence end.
const MIN_CHUNK: usize = 250;

/// Words as whitespace-separated tokens.
pub fn word_count(text: &str) -> i64 {
    text.split_whitespace().count() as i64
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Plain text paragraphs from a text file: paragraphs are separated by blank
/// lines, and hard-wrapped lines inside a paragraph are joined with a space.
pub fn paragraphs_from_plain(text: &str) -> Vec<String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                out.push(collapse(&cur.join(" ")));
                cur.clear();
            }
        } else {
            cur.push(line);
        }
    }
    if !cur.is_empty() {
        out.push(collapse(&cur.join(" ")));
    }
    out.retain(|p| !p.is_empty());
    out
}

fn decode_entity(name: &str) -> Option<String> {
    if let Some(num) = name.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse().ok()?
        };
        return char::from_u32(code).map(|c| c.to_string());
    }
    Some(
        match name {
            "amp" => "&",
            "lt" => "<",
            "gt" => ">",
            "quot" => "\"",
            "apos" => "'",
            "nbsp" => " ",
            "mdash" => "\u{2014}",
            "ndash" => "\u{2013}",
            "hellip" => "\u{2026}",
            "lsquo" => "\u{2018}",
            "rsquo" => "\u{2019}",
            "ldquo" => "\u{201C}",
            "rdquo" => "\u{201D}",
            "copy" => "\u{00A9}",
            "eacute" => "\u{00E9}",
            "egrave" => "\u{00E8}",
            "agrave" => "\u{00E0}",
            "uuml" => "\u{00FC}",
            "ouml" => "\u{00F6}",
            "auml" => "\u{00E4}",
            "ccedil" => "\u{00E7}",
            "ntilde" => "\u{00F1}",
            _ => return None,
        }
        .to_string(),
    )
}

const BLOCKS: &[&str] = &[
    "p",
    "div",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "li",
    "blockquote",
    "tr",
    "section",
    "article",
    "pre",
    "ul",
    "ol",
    "table",
    "hr",
    "aside",
    "figure",
    "figcaption",
    "dd",
    "dt",
    "body",
];
const SKIPPED: &[&str] = &["script", "style", "head", "svg", "nav"];

/// What a chapter's markup yields.
pub struct Extracted {
    /// First heading (h1 to h3), else the document title.
    pub title: Option<String>,
    pub paragraphs: Vec<String>,
}

/// A forgiving markup reader: tolerates malformed XHTML and HTML, drops scripts,
/// styles, headers and navigation, and turns block elements into paragraphs.
pub fn extract_markup(html: &str) -> Extracted {
    let chars: Vec<char> = html.chars().collect();
    let mut paragraphs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut heading_title: Option<String> = None;
    let mut doc_title: Option<String> = None;
    let mut skip_depth: Option<String> = None;
    let mut in_heading: Option<String> = None;
    let mut heading_buf = String::new();
    let mut in_title = false;
    let mut in_head = false;
    let mut title_buf = String::new();
    let mut i = 0;

    let flush = |cur: &mut String, paragraphs: &mut Vec<String>| {
        let t = collapse(cur);
        if !t.is_empty() {
            paragraphs.push(t);
        }
        cur.clear();
    };

    while i < chars.len() {
        let c = chars[i];
        if c == '<' {
            // Comment, CDATA, doctype, declaration.
            if chars[i..].starts_with(&['<', '!', '-', '-']) {
                let end = find(&chars, i + 4, &['-', '-', '>'])
                    .map(|p| p + 3)
                    .unwrap_or(chars.len());
                i = end;
                continue;
            }
            let Some(close) = chars[i..].iter().position(|&ch| ch == '>') else {
                break;
            };
            let raw: String = chars[i + 1..i + close].iter().collect();
            i += close + 1;
            let closing = raw.starts_with('/');
            let name: String = raw
                .trim_start_matches('/')
                .chars()
                .take_while(|ch| !ch.is_whitespace() && *ch != '/' && *ch != '>')
                .collect::<String>()
                .to_lowercase();
            let self_closing = raw.ends_with('/');
            if name.is_empty() || name.starts_with(['!', '?']) {
                continue;
            }
            if let Some(skip) = &skip_depth {
                if closing && &name == skip {
                    skip_depth = None;
                }
                continue;
            }
            if !closing && !self_closing && SKIPPED.contains(&name.as_str()) && name != "head" {
                skip_depth = Some(name);
                continue;
            }
            if name == "head" {
                in_head = !closing && !self_closing;
                if !in_head {
                    in_title = false;
                }
                continue;
            }
            if name == "body" && !closing {
                // Recover from an unclosed title/head in converted HTML.
                in_head = false;
                in_title = false;
            }
            if name == "title" {
                in_title = !closing && !self_closing;
                if closing && doc_title.is_none() {
                    let t = collapse(&title_buf);
                    if !t.is_empty() {
                        doc_title = Some(t);
                    }
                }
                continue;
            }
            if in_head {
                continue;
            }
            if name == "br" {
                cur.push(' ');
                continue;
            }
            let is_heading = matches!(name.as_str(), "h1" | "h2" | "h3");
            if BLOCKS.contains(&name.as_str()) {
                flush(&mut cur, &mut paragraphs);
            }
            if is_heading {
                if !closing {
                    in_heading = Some(name.clone());
                    heading_buf.clear();
                } else if in_heading.as_deref() == Some(name.as_str()) {
                    let t = collapse(&heading_buf);
                    if heading_title.is_none() && !t.is_empty() {
                        heading_title = Some(t);
                    }
                    in_heading = None;
                }
            }
            continue;
        }
        if skip_depth.is_some() || (in_head && !in_title) {
            i += 1;
            continue;
        }
        let mut piece = String::new();
        if c == '&' {
            // Entity: up to 10 chars before a semicolon.
            if let Some(semi) = chars[i + 1..(i + 12).min(chars.len())]
                .iter()
                .position(|&ch| ch == ';')
            {
                let name: String = chars[i + 1..i + 1 + semi].iter().collect();
                if let Some(d) = decode_entity(&name) {
                    piece = d;
                    i += semi + 2;
                }
            }
        }
        if piece.is_empty() {
            piece.push(c);
            i += 1;
        }
        if in_title {
            title_buf.push_str(&piece);
        } else {
            cur.push_str(&piece);
            if in_heading.is_some() {
                heading_buf.push_str(&piece);
            }
        }
    }
    flush(&mut cur, &mut paragraphs);
    Extracted {
        title: heading_title.or(doc_title),
        paragraphs,
    }
}

fn find(hay: &[char], from: usize, needle: &[char]) -> Option<usize> {
    (from..hay.len().saturating_sub(needle.len() - 1)).find(|&p| hay[p..].starts_with(needle))
}

/// Canonical chapter text and its lines, as `(start, end)` code point spans.
pub fn build_chapter(paragraphs: &[String]) -> (String, Vec<(usize, usize)>) {
    let mut text = String::new();
    let mut lines = Vec::new();
    let mut offset = 0usize; // in chars
    for (n, p) in paragraphs.iter().enumerate() {
        if n > 0 {
            text.push_str("\n\n");
            offset += 2;
        }
        for (s, e) in split_paragraph(p) {
            lines.push((offset + s, offset + e));
        }
        text.push_str(p);
        offset += p.chars().count();
    }
    (text, lines)
}

/// Spans inside one paragraph: the whole paragraph, or sentence runs when it is long.
fn split_paragraph(p: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = p.chars().collect();
    let n = chars.len();
    if n <= MAX_LINE {
        return vec![(0, n)];
    }
    let mut spans = Vec::new();
    let mut start = 0usize;
    while start < n {
        let remaining = n - start;
        if remaining <= MAX_LINE {
            spans.push((start, n));
            break;
        }
        // Prefer the last sentence end within the window after the minimum chunk.
        let window_end = start + MAX_LINE;
        let mut cut = None;
        let mut j = start + MIN_CHUNK;
        while j < window_end {
            let c = chars[j];
            if matches!(c, '.' | '!' | '?' | '\u{2026}') {
                let mut k = j + 1;
                while k < n && matches!(chars[k], '"' | '\'' | '\u{201D}' | '\u{2019}' | ')') {
                    k += 1;
                }
                if k < n && chars[k] == ' ' {
                    cut = Some(k);
                }
            }
            j += 1;
        }
        let end = cut.unwrap_or_else(|| {
            // No sentence end: cut at the last space in the window, else hard.
            (start + MIN_CHUNK..window_end)
                .rev()
                .find(|&x| chars[x] == ' ')
                .unwrap_or(window_end)
        });
        spans.push((start, end));
        // Skip the space that separated the runs so lines start on text.
        start = end;
        while start < n && chars[start] == ' ' {
            start += 1;
        }
    }
    spans
}

/// Heuristic chapter kind from a title.
pub fn kind_from_title(title: &str) -> &'static str {
    let t = collapse(title).to_lowercase();
    let t = t.trim_matches(|c: char| c.is_whitespace() || matches!(c, '.' | ':' | '-' | '—' | '–'));
    const FRONT: &[&str] = &[
        "cover",
        "cover page",
        "toc",
        "title page",
        "titlepage",
        "table of contents",
        "contents",
        "copyright",
        "dedication",
        "epigraph",
        "imprint",
        "half title",
        "halftitlepage",
        "front matter",
        "foreword",
        "preface",
        "introduction",
    ];
    const BACK: &[&str] = &[
        "acknowledgments",
        "acknowledgements",
        "about the author",
        "colophon",
        "also by",
        "other books",
        "author's note",
        "praise for",
        "back matter",
        "afterword",
        "appendix",
        "appendices",
        "bibliography",
        "glossary",
        "index",
    ];
    // Exact labels, a subtitle separator or a year/number are evidence of
    // matter. Ordinary prose such as "Cover the lantern" is not a heading.
    let matches = |k: &&str| {
        t.strip_prefix(*k)
            .map(|rest| {
                let word_suffix = rest.starts_with(char::is_whitespace);
                let rest = rest.trim_start();
                rest.is_empty()
                    || rest.starts_with(|c: char| {
                        c.is_numeric() || matches!(c, ':' | '-' | '—' | '–' | '·' | '(' | '©')
                    })
                    || matches!(*k, "also by" | "other books" | "praise for") && word_suffix
            })
            .unwrap_or(false)
    };
    if FRONT.iter().any(matches) {
        "front_matter"
    } else if BACK.iter().any(matches) {
        "back_matter"
    } else {
        "story"
    }
}

/// Split plain-text paragraphs into chapters at headings. A heading is a short
/// paragraph such as `Chapter 3`, `CHAPTER IV: The River`, `Part Two`, `Prologue`,
/// or a lone roman numeral, when at least two such headings exist.
pub fn split_plain_chapters(
    paragraphs: Vec<String>,
    fallback_title: &str,
) -> Vec<(String, Vec<String>)> {
    let is_word_heading = |p: &str| {
        let l = p.to_lowercase();
        p.chars().count() <= 80
            && (kind_from_title(p) != "story"
                || [
                    "chapter ",
                    "part ",
                    "book ",
                    "prologue",
                    "epilogue",
                    "interlude",
                    "section ",
                ]
                .iter()
                .any(|k| l.starts_with(k) || l == k.trim()))
    };
    let is_roman = |p: &str| {
        let t = p.trim_end_matches('.');
        !t.is_empty() && t.len() <= 8 && t.chars().all(|c| "IVXLCDM".contains(c))
    };
    let word_headings = paragraphs.iter().filter(|p| is_word_heading(p)).count();
    let roman_headings = paragraphs.iter().filter(|p| is_roman(p)).count();
    let use_roman = word_headings < 2 && roman_headings >= 2;
    if word_headings < 2 && !use_roman {
        return vec![(fallback_title.to_string(), paragraphs)];
    }
    let mut chapters: Vec<(String, Vec<String>)> = Vec::new();
    let mut preface: Vec<String> = Vec::new();
    for p in paragraphs {
        let heading = if use_roman {
            is_roman(&p)
        } else {
            is_word_heading(&p)
        };
        if heading {
            // A heading is display metadata and also source text. Keep it in
            // the spoken/read text just as EPUB headings are kept.
            chapters.push((p.clone(), vec![p]));
        } else if let Some(last) = chapters.last_mut() {
            last.1.push(p);
        } else {
            preface.push(p);
        }
    }
    if !preface.is_empty() {
        chapters.insert(0, ("Beginning".to_string(), preface));
    }
    chapters.retain(|(_, ps)| !ps.is_empty());
    chapters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paragraphs_join_hard_wraps() {
        let p = paragraphs_from_plain("One two\nthree.\n\n\nFour.\r\n\r\nFive\n");
        assert_eq!(p, ["One two three.", "Four.", "Five"]);
    }

    #[test]
    fn markup_becomes_paragraphs_and_decodes_entities() {
        let e = extract_markup(
            "<html><head><title>Doc</title><style>p{}</style></head><body><h2>The Ferry</h2>\
             <p>It was &ldquo;late&rdquo; &amp; cold&#8230;</p><script>x()</script><p>Second<br/>line &#x41;.</p></body></html>",
        );
        assert_eq!(e.title.as_deref(), Some("The Ferry"));
        assert_eq!(
            e.paragraphs,
            [
                "The Ferry",
                "It was \u{201C}late\u{201D} & cold\u{2026}",
                "Second line A."
            ]
        );
    }

    #[test]
    fn malformed_markup_does_not_panic() {
        let e = extract_markup("<p>open <b>bold <i>never closed &unknown; & loose < less than");
        assert!(!e.paragraphs.is_empty());
        let _ = extract_markup("<<<>>>&;&#;&#xZZ;<!-- unterminated");
    }

    #[test]
    fn offsets_are_code_points_not_bytes() {
        let paras = vec![
            "h\u{00E9}llo \u{1F600} w\u{00F6}rld".to_string(),
            "second".to_string(),
        ];
        let (text, lines) = build_chapter(&paras);
        assert_eq!(lines.len(), 2);
        let chars: Vec<char> = text.chars().collect();
        let first: String = chars[lines[0].0..lines[0].1].iter().collect();
        let second: String = chars[lines[1].0..lines[1].1].iter().collect();
        assert_eq!(first, paras[0]);
        assert_eq!(second, "second");
        // Bytes differ from code points for this text.
        assert_ne!(text.len(), chars.len());
    }

    #[test]
    fn long_paragraphs_split_at_sentence_ends_and_cover_the_text() {
        let sentence = "The river ran slow and brown past the old stone pier. ";
        let para: String = sentence.repeat(30).trim_end().to_string();
        let (text, lines) = build_chapter(std::slice::from_ref(&para));
        assert!(lines.len() > 1);
        let chars: Vec<char> = text.chars().collect();
        let mut prev_end = 0;
        for (s, e) in &lines {
            assert!(*s >= prev_end && e > s && e - s <= MAX_LINE, "{s}..{e}");
            let piece: String = chars[*s..*e].iter().collect();
            assert!(piece.ends_with('.'), "cut at a sentence end: {piece:?}");
            prev_end = *e;
        }
        assert_eq!(lines.last().unwrap().1, chars.len());
    }

    #[test]
    fn a_long_paragraph_with_no_sentence_ends_is_still_cut() {
        let para = "word ".repeat(400);
        let (_, lines) = build_chapter(&[para.trim_end().to_string()]);
        assert!(lines.iter().all(|(s, e)| e - s <= MAX_LINE));
    }

    #[test]
    fn chapters_are_split_at_headings() {
        let paras: Vec<String> = [
            "A title page line",
            "Chapter 1",
            "Body one.",
            "Chapter 2: The River",
            "Body two.",
            "More.",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let ch = split_plain_chapters(paras, "Text");
        let titles: Vec<&str> = ch.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(titles, ["Beginning", "Chapter 1", "Chapter 2: The River"]);
        assert_eq!(ch[2].1, ["Chapter 2: The River", "Body two.", "More."]);
    }

    #[test]
    fn no_headings_means_one_chapter() {
        let ch = split_plain_chapters(vec!["Just a story.".into()], "Text");
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].0, "Text");
    }

    #[test]
    fn kinds_from_titles() {
        assert_eq!(kind_from_title("Copyright"), "front_matter");
        assert_eq!(kind_from_title("Table of Contents"), "front_matter");
        assert_eq!(kind_from_title("Acknowledgements"), "back_matter");
        assert_eq!(kind_from_title("Chapter 1: Ash"), "story");
        assert_eq!(
            kind_from_title("Preface: Before the crossing"),
            "front_matter"
        );
        assert_eq!(kind_from_title("Afterword"), "back_matter");
        assert_eq!(kind_from_title("Copyrighted River"), "story");
        assert_eq!(kind_from_title("Covering the Distance"), "story");
        assert_eq!(
            kind_from_title("The Acknowledgements of a Stranger"),
            "story"
        );
        assert_eq!(kind_from_title("Chapter 3: About the Author"), "story");
    }

    #[test]
    fn document_title_is_metadata_and_heading_wins() {
        let e = extract_markup("<html><head><title>The &amp; Crossing</title></head><body><p>Words stay here.</p></body></html>");
        assert_eq!(e.title.as_deref(), Some("The & Crossing"));
        assert_eq!(e.paragraphs, ["Words stay here."]);
        let e = extract_markup("<html><head><title>Generic</title></head><body><h2>Real chapter</h2><p>Words.</p></body></html>");
        assert_eq!(e.title.as_deref(), Some("Real chapter"));
        for html in [
            "<html><head><title/></head><body><p>Words stay here.</p></body></html>",
            "<html><head><title>Unclosed</head><body><p>Words stay here.</p></body></html>",
            "<html><head><title>Unclosed<body><p>Words stay here.</p></body></html>",
        ] {
            assert_eq!(extract_markup(html).paragraphs, ["Words stay here."]);
        }
    }

    #[test]
    fn plain_matter_and_story_headings_keep_every_word() {
        let original = "Copyright\n\nAn original notice.\n\nChapter 1: Café 😀\n\nThe ferry moved.\n\nAcknowledgements\n\nThanks to the crew.";
        let paragraphs = paragraphs_from_plain(original);
        let chapters = split_plain_chapters(paragraphs.clone(), "Ferry");
        assert_eq!(chapters.len(), 3);
        let joined: Vec<_> = chapters
            .iter()
            .flat_map(|(_, ps)| ps.iter().cloned())
            .collect();
        assert_eq!(joined, paragraphs);
    }

    #[test]
    fn prose_starting_with_a_matter_word_stays_in_the_story() {
        let paragraphs = [
            "Chapter 1",
            "Cover the lantern before the rain comes.",
            "Index the parcels before dawn.",
            "Introduction of the new ferry changed the town.",
            "Chapter 2",
            "The lantern glowed.",
        ]
        .map(str::to_string)
        .to_vec();
        let chapters = split_plain_chapters(paragraphs.clone(), "Ferry");
        assert_eq!(chapters.len(), 2);
        assert!(paragraphs[1..4]
            .iter()
            .all(|p| kind_from_title(p) == "story"));
        assert_eq!(chapters[0].1, paragraphs[..4]);
    }
}
