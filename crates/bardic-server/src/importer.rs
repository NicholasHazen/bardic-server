//! Reading a book file into chapters. Pure: no database, no network.
//!
//! Limits (the contract's): EPUBs may expand to at most 100 MB and 5,000 files.
//! DRM-protected books are refused, never partially read.

use crate::text::{
    build_chapter, extract_markup, kind_from_title, paragraphs_from_plain, split_plain_chapters,
};
use std::io::{Cursor, Read};
use zip::ZipArchive;

mod structure;

const MAX_ENTRIES: usize = 5_000;
const MAX_EXPANDED: u64 = 100 * 1024 * 1024;
const MAX_ENTRY: u64 = 30 * 1024 * 1024;
/// Most text one book may hold once markup is removed (a long novel is about 1 MB).
const MAX_BOOK_TEXT: usize = 100 * 1024 * 1024;

pub struct ParsedChapter {
    pub title: String,
    pub kind: &'static str,
    pub text: String,
    pub lines: Vec<(usize, usize)>,
    pub page_count: Option<i64>,
}

pub struct ParsedBook {
    pub title: String,
    pub author: String,
    pub chapters: Vec<ParsedChapter>,
    pub cover: Option<Vec<u8>>,
    pub ext: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportFailure {
    Drm,
    Unreadable(String),
    UnsupportedEncoding(String),
    NoText,
}

impl ImportFailure {
    pub fn code(&self) -> &'static str {
        match self {
            ImportFailure::Drm => "import_drm_protected",
            ImportFailure::Unreadable(_) => "import_unreadable",
            ImportFailure::UnsupportedEncoding(_) => "import_unsupported_encoding",
            ImportFailure::NoText => "import_no_text",
        }
    }
    pub fn detail(&self) -> String {
        match self {
            ImportFailure::Drm => {
                "This book is protected by DRM. Bardic cannot open it. Use a DRM-free copy.".into()
            }
            ImportFailure::Unreadable(why) => format!("The file could not be read: {why}"),
            ImportFailure::UnsupportedEncoding(why) => {
                format!("The text is not readable as UTF-8: {why}")
            }
            ImportFailure::NoText => "The file has no readable text.".into(),
        }
    }
}

fn unreadable(s: impl Into<String>) -> ImportFailure {
    ImportFailure::Unreadable(s.into())
}

/// Title from a file name: no extension, underscores as spaces.
pub fn title_from_file_name(file_name: &str) -> String {
    let stem = file_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(file_name);
    let t = stem.replace('_', " ");
    let t = t.trim();
    if t.is_empty() {
        "Untitled".to_string()
    } else {
        t.to_string()
    }
}

pub fn extension(file_name: &str) -> Option<&'static str> {
    let lower = file_name.to_lowercase();
    if lower.ends_with(".epub") {
        Some("epub")
    } else if lower.ends_with(".txt") {
        Some("txt")
    } else {
        None
    }
}

pub fn parse(file_name: &str, bytes: &[u8]) -> Result<ParsedBook, ImportFailure> {
    if bytes.is_empty() {
        return Err(ImportFailure::NoText);
    }
    match extension(file_name) {
        Some("epub") => parse_epub(file_name, bytes),
        Some("txt") => parse_txt(file_name, bytes),
        _ => Err(unreadable("only .epub and .txt files can be added")),
    }
}

// ------------------------------------------------------------------- text

pub fn parse_txt(file_name: &str, bytes: &[u8]) -> Result<ParsedBook, ImportFailure> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes.contains(&0) {
        return Err(ImportFailure::UnsupportedEncoding(
            "the file looks binary".into(),
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ImportFailure::UnsupportedEncoding(e.to_string()))?;
    let title = title_from_file_name(file_name);
    let paragraphs = paragraphs_from_plain(text);
    if paragraphs.is_empty() {
        return Err(ImportFailure::NoText);
    }
    let chapters = split_plain_chapters(paragraphs, &title)
        .into_iter()
        .map(|(t, ps)| {
            let (text, lines) = build_chapter(&ps);
            ParsedChapter {
                kind: kind_from_title(&t),
                title: t,
                text,
                lines,
                page_count: None,
            }
        })
        .collect();
    Ok(ParsedBook {
        title,
        author: String::new(),
        chapters,
        cover: None,
        ext: "txt",
    })
}

// ------------------------------------------------------------------- epub

fn read_entry<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    name: &str,
    cap: u64,
) -> Result<Vec<u8>, ImportFailure> {
    let f = zip
        .by_name(name)
        .map_err(|_| unreadable(format!("missing {name}")))?;
    let mut buf = Vec::new();
    f.take(cap + 1)
        .read_to_end(&mut buf)
        .map_err(|e| unreadable(e.to_string()))?;
    if buf.len() as u64 > cap {
        return Err(unreadable(format!("{name} is too large")));
    }
    Ok(buf)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve `href` against the folder of the OPF, without escaping the archive.
fn resolve(base_dir: &str, href: &str) -> String {
    let href = percent_decode(href.split('#').next().unwrap_or(""));
    let mut parts: Vec<&str> = if href.starts_with('/') {
        Vec::new()
    } else {
        base_dir.split('/').filter(|p| !p.is_empty()).collect()
    };
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn local<'a>(n: &roxmltree::Node<'a, 'a>) -> &'a str {
    n.tag_name().name()
}

pub fn parse_epub(file_name: &str, bytes: &[u8]) -> Result<ParsedBook, ImportFailure> {
    let mut zip = ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| unreadable(format!("not a valid EPUB: {e}")))?;
    if zip.len() > MAX_ENTRIES {
        return Err(unreadable("the EPUB has too many files"));
    }
    let mut expanded = 0u64;
    for i in 0..zip.len() {
        expanded = expanded.saturating_add(
            zip.by_index(i)
                .map_err(|e| unreadable(e.to_string()))?
                .size(),
        );
    }
    if expanded > MAX_EXPANDED {
        return Err(unreadable("the EPUB is too large when expanded"));
    }

    // DRM: anything encrypted other than font obfuscation.
    if let Ok(enc) = read_entry(&mut zip, "META-INF/encryption.xml", 1 << 20) {
        if let Ok(text) = std::str::from_utf8(&enc) {
            if let Ok(doc) = roxmltree::Document::parse(text) {
                const FONT: [&str; 2] = [
                    "http://www.idpf.org/2008/embedding",
                    "http://ns.adobe.com/pdf/enc#RC",
                ];
                let protected = doc
                    .descendants()
                    .filter(|n| local(n) == "EncryptionMethod")
                    .any(|n| {
                        n.attribute("Algorithm")
                            .map(|a| !FONT.contains(&a))
                            .unwrap_or(true)
                    });
                if protected {
                    return Err(ImportFailure::Drm);
                }
            }
        }
    }

    let container = read_entry(&mut zip, "META-INF/container.xml", 1 << 20)?;
    let container =
        std::str::from_utf8(&container).map_err(|_| unreadable("container.xml is not text"))?;
    let cdoc = roxmltree::Document::parse(container)
        .map_err(|e| unreadable(format!("container.xml: {e}")))?;
    let opf_path = cdoc
        .descendants()
        .find(|n| local(n) == "rootfile")
        .and_then(|n| n.attribute("full-path"))
        .ok_or_else(|| unreadable("no package document"))?
        .to_string();
    let opf_dir = opf_path
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();
    let opf_bytes = read_entry(&mut zip, &opf_path, 5 << 20)?;
    let opf_text = String::from_utf8_lossy(&opf_bytes).into_owned();
    let opf = roxmltree::Document::parse_with_options(
        &opf_text,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .map_err(|e| unreadable(format!("package document: {e}")))?;

    let text_of = |name: &str| -> Vec<String> {
        opf.descendants()
            .filter(|n| {
                local(n) == name
                    && n.tag_name()
                        .namespace()
                        .map(|ns| ns.contains("purl.org/dc"))
                        .unwrap_or(false)
            })
            .filter_map(|n| n.text().map(|t| t.trim().to_string()))
            .filter(|t| !t.is_empty())
            .collect()
    };
    let title = text_of("title")
        .into_iter()
        .next()
        .unwrap_or_else(|| title_from_file_name(file_name));
    let author = text_of("creator").join(", ");

    struct Item {
        href: String,
        media: String,
        props: String,
    }
    let manifest: std::collections::HashMap<String, Item> = opf
        .descendants()
        .filter(|n| local(n) == "item")
        .filter_map(|n| {
            Some((
                n.attribute("id")?.to_string(),
                Item {
                    href: n.attribute("href")?.to_string(),
                    media: n.attribute("media-type").unwrap_or("").to_string(),
                    props: n.attribute("properties").unwrap_or("").to_string(),
                },
            ))
        })
        .collect();

    // Navigation is display metadata over the spine, never a second reading
    // order or a reason to rewrite chapter text. Optional malformed metadata
    // falls back to headings and names; no provider participates in import.
    let mut structure = structure::Structure::default();
    structure.add_guide(&opf_path, &opf_text);
    let mut items: Vec<_> = manifest.iter().collect();
    let ncx_id = opf
        .descendants()
        .find(|n| local(n) == "spine")
        .and_then(|n| n.attribute("toc"));
    items.sort_by_key(|(id, _)| (Some(id.as_str()) != ncx_id, id.as_str()));
    for (_, item) in items {
        let path = resolve(&opf_dir, &item.href);
        if item.props.split_whitespace().any(|p| p == "nav") {
            if let Ok(raw) = read_entry(&mut zip, &path, MAX_ENTRY) {
                structure.add_navigation(&path, &String::from_utf8_lossy(&raw));
            }
        } else if item.media == "application/x-dtbncx+xml" {
            if let Ok(raw) = read_entry(&mut zip, &path, MAX_ENTRY) {
                structure.add_ncx(&path, &String::from_utf8_lossy(&raw));
            }
        }
    }

    // Cover: <meta name="cover" content="id"> or properties="cover-image".
    let cover_id = opf
        .descendants()
        .find(|n| local(n) == "meta" && n.attribute("name") == Some("cover"))
        .and_then(|n| n.attribute("content"))
        .map(str::to_string)
        .or_else(|| {
            manifest
                .iter()
                .find(|(_, it)| it.props.split_whitespace().any(|p| p == "cover-image"))
                .map(|(id, _)| id.clone())
        });
    let cover = cover_id
        .and_then(|id| manifest.get(&id))
        .and_then(|it| read_entry(&mut zip, &resolve(&opf_dir, &it.href), 20 << 20).ok());

    let spine: Vec<String> = opf
        .descendants()
        .filter(|n| local(n) == "itemref")
        .filter_map(|n| n.attribute("idref").map(str::to_string))
        .collect();
    let mut chapters: Vec<ParsedChapter> = Vec::new();
    let mut story_n = 0;
    let mut front_n = 0;
    let mut back_n = 0;
    // An entry listed many times in the spine is read once, and the text of a book is capped,
    // so a small file cannot expand into gigabytes of chapters.
    let mut seen = std::collections::HashSet::new();
    let mut text_bytes = 0usize;
    for idref in spine {
        if !seen.insert(idref.clone()) {
            continue;
        }
        if text_bytes > MAX_BOOK_TEXT {
            break;
        }
        let Some(item) = manifest.get(&idref) else {
            continue;
        };
        if !item.media.contains("html") || item.props.split_whitespace().any(|p| p == "nav") {
            continue;
        }
        let path = resolve(&opf_dir, &item.href);
        let Ok(raw) = read_entry(&mut zip, &path, MAX_ENTRY) else {
            continue;
        };
        let raw = String::from_utf8_lossy(&raw);
        let ex = extract_markup(&raw);
        if ex.paragraphs.is_empty() {
            continue;
        }
        text_bytes += ex.paragraphs.iter().map(String::len).sum::<usize>();
        let (navigation_title, structural_kind) = structure.chapter_metadata(&path, &raw);
        let title = navigation_title.or(ex.title);
        let unnamed = title.is_none();
        let hint = title
            .clone()
            .unwrap_or_else(|| idref.replace(['_', '-'], " "));
        let mut kind = structural_kind.unwrap_or_else(|| kind_from_title(&hint));
        if structural_kind.is_none() && unnamed && kind == "story" {
            // File names such as cover.xhtml or toc.xhtml mark matter even without a heading.
            kind = kind_from_title(
                &item
                    .href
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(".xhtml")
                    .trim_end_matches(".html")
                    .replace(['_', '-'], " "),
            );
        }
        let fallback = match kind {
            "front_matter" => {
                front_n += 1;
                format!("Front matter {front_n}")
            }
            "back_matter" => {
                back_n += 1;
                format!("Back matter {back_n}")
            }
            _ => {
                story_n += 1;
                format!("Chapter {story_n}")
            }
        };
        let title = match title {
            Some(t) => t,
            None => fallback,
        };
        let (text, lines) = build_chapter(&ex.paragraphs);
        chapters.push(ParsedChapter {
            title,
            kind,
            text,
            lines,
            page_count: structure.page_count(&path, &raw),
        });
    }
    if chapters.iter().all(|c| c.text.trim().is_empty()) {
        return Err(ImportFailure::NoText);
    }
    Ok(ParsedBook {
        title,
        author,
        chapters,
        cover,
        ext: "epub",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn structured_epub(manifest: &str, spine: &str, files: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut z = ZipWriter::new(Cursor::new(&mut out));
            let o = SimpleFileOptions::default();
            z.start_file("META-INF/container.xml", o).unwrap();
            z.write_all(br#"<container><rootfiles><rootfile full-path="OPS/content.opf"/></rootfiles></container>"#).unwrap();
            z.start_file("OPS/content.opf", o).unwrap();
            z.write_all(format!(r#"<package><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>The Lantern Ferry</dc:title></metadata><manifest>{manifest}</manifest>{spine}</package>"#).as_bytes()).unwrap();
            for (path, text) in files {
                z.start_file(*path, o).unwrap();
                z.write_all(text.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        out
    }

    #[test]
    fn epub_navigation_names_and_semantics_follow_spine_without_changing_text() {
        let manifest = r#"<item id="nav" href="nav/toc.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="c0" href="text/c0.xhtml" media-type="application/xhtml+xml"/><item id="c1" href="text/café 1.xhtml" media-type="application/xhtml+xml"/><item id="c2" href="text/c2.xhtml" media-type="application/xhtml+xml"/><item id="c3" href="text/c3.xhtml" media-type="application/xhtml+xml"/>"#;
        let bytes = structured_epub(manifest, r#"<spine><itemref idref="c0"/><itemref idref="c1"/><itemref idref="c2"/><itemref idref="c3"/></spine>"#, &[
            ("OPS/nav/toc.xhtml", r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="page-list"><a href="../text/c2.xhtml">Page 2</a></nav><nav epub:type="toc"><ol><li><a href="../text/c2.xhtml">The Far Shore</a></li><li><a href="../text/caf%C3%A9%201.xhtml#start">Chapter 1: Café &amp; Lanterns</a></li><li><a href="../text/c0.xhtml">Before the voyage</a></li><li><a href="../text/c3.xhtml">After the voyage</a></li></ol></nav></body></html>"#),
            ("OPS/text/c0.xhtml", r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body epub:type="frontmatter"><p>An original notice.</p></body></html>"#),
            ("OPS/text/café 1.xhtml", r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body epub:type="chapter"><h1 id="start">One</h1><p>The café bell rang 😀.</p></body></html>"#),
            ("OPS/text/c2.xhtml", "<html><head><title>Generic</title></head><body><p>The ferry arrived.</p></body></html>"),
            ("OPS/text/c3.xhtml", "<html><body role=\"doc-acknowledgments\"><p>Thanks to the crew.</p></body></html>"),
        ]);
        let book = parse("lantern.epub", &bytes).unwrap();
        let titles: Vec<_> = book.chapters.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Before the voyage",
                "Chapter 1: Café & Lanterns",
                "The Far Shore",
                "After the voyage"
            ]
        );
        let kinds: Vec<_> = book.chapters.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, ["front_matter", "story", "story", "back_matter"]);
        assert_eq!(book.chapters[1].text, "One\n\nThe café bell rang 😀.");
        let chars: Vec<_> = book.chapters[1].text.chars().collect();
        let (start, end) = book.chapters[1].lines[1];
        assert_eq!(
            chars[start..end].iter().collect::<String>(),
            "The café bell rang 😀."
        );
    }

    #[test]
    fn broken_navigation_falls_back_to_ncx_and_unknown_names_use_chapters() {
        let manifest = r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="ncx" href="nav/toc.ncx" media-type="application/x-dtbncx+xml"/><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/><item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/>"#;
        let bytes = structured_epub(
            manifest,
            r#"<spine toc="ncx"><itemref idref="c1"/><itemref idref="c2"/></spine>"#,
            &[
                ("OPS/nav.xhtml", "<broken"),
                (
                    "OPS/nav/toc.ncx",
                    r#"<ncx><navMap><navPoint><navLabel><text>The Crossing</text></navLabel><content src="../c1.xhtml"/></navPoint></navMap></ncx>"#,
                ),
                (
                    "OPS/c1.xhtml",
                    "<html><body><p>A lantern glowed.</p></body></html>",
                ),
                (
                    "OPS/c2.xhtml",
                    "<html><body><p>The river turned.</p></body></html>",
                ),
            ],
        );
        let book = parse("lantern.epub", &bytes).unwrap();
        assert_eq!(book.chapters[0].title, "The Crossing");
        assert_eq!(book.chapters[1].title, "Chapter 2");
        assert!(book.chapters.iter().all(|c| c.kind == "story"));
    }

    #[test]
    fn a_named_story_is_not_matter_just_because_of_its_filename() {
        let bytes = structured_epub(
            r#"<item id="c1" href="index.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<spine><itemref idref="c1"/></spine>"#,
            &[(
                "OPS/index.xhtml",
                "<html><body><h1>Chapter 1: The Crossing</h1><p>The ferry left.</p></body></html>",
            )],
        );
        let book = parse("lantern.epub", &bytes).unwrap();
        assert_eq!(book.chapters[0].title, "Chapter 1: The Crossing");
        assert_eq!(book.chapters[0].kind, "story");
    }

    #[test]
    fn converted_document_identifiers_use_matter_fallback_without_changing_words() {
        let bytes = structured_epub(
            r#"<item id="cD" href="dedication.xhtml" media-type="application/xhtml+xml"/><item id="c1" href="chapter01.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<spine><itemref idref="cD"/><itemref idref="c1"/></spine>"#,
            &[
                ("OPS/dedication.xhtml", "<html><head><title>cD</title></head><body><p>For the invented crew.</p></body></html>"),
                ("OPS/chapter01.xhtml", "<html><head><title>chapter01</title></head><body><h1>cD</h1><h2>Chapter 1: The Crossing</h2><p>The ferry left.</p></body></html>"),
            ],
        );
        let book = parse("lantern.epub", &bytes).unwrap();
        assert_eq!(book.chapters[0].title, "Front matter 1");
        assert_eq!(book.chapters[0].kind, "front_matter");
        assert_eq!(book.chapters[0].text, "For the invented crew.");
        assert_eq!(book.chapters[1].title, "Chapter 1: The Crossing");
        assert_eq!(book.chapters[1].kind, "story");
        assert_eq!(
            book.chapters[1].text,
            "cD\n\nChapter 1: The Crossing\n\nThe ferry left."
        );
    }

    #[test]
    fn sibling_navigation_in_one_mixed_source_unit_keeps_the_story_eligible() {
        let bytes = structured_epub(
            r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="c1" href="mixed.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<spine><itemref idref="c1"/></spine>"#,
            &[
                (
                    "OPS/nav.xhtml",
                    r#"<nav role="doc-toc"><ol><li><a href="mixed.xhtml#rights">Copyright</a></li><li><a href="mixed.xhtml#story">The Crossing</a></li></ol></nav>"#,
                ),
                (
                    "OPS/mixed.xhtml",
                    r#"<html><body><h1 id="rights">Copyright</h1><p>An original notice.</p><h1 id="story">The Crossing</h1><p>The ferry left.</p></body></html>"#,
                ),
            ],
        );
        let book = parse("lantern.epub", &bytes).unwrap();
        assert_eq!(book.chapters.len(), 1);
        assert_eq!(book.chapters[0].kind, "story");
        assert_eq!(
            book.chapters[0].text,
            "Copyright\n\nAn original notice.\n\nThe Crossing\n\nThe ferry left."
        );
    }

    #[test]
    fn mixed_semantic_sections_without_navigation_keep_the_story_eligible() {
        let bytes = structured_epub(
            r#"<item id="c1" href="mixed.xhtml" media-type="application/xhtml+xml"/>"#,
            r#"<spine><itemref idref="c1"/></spine>"#,
            &[(
                "OPS/mixed.xhtml",
                r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body><section epub:type="copyright-page"><h1>Copyright</h1><p>An original notice.</p></section><section epub:type="chapter"><h1>The Crossing</h1><p>Mira rowed home 😀.</p></section></body></html>"#,
            )],
        );
        let book = parse("lantern.epub", &bytes).unwrap();
        assert_eq!(book.chapters.len(), 1);
        assert_eq!(book.chapters[0].title, "Copyright");
        assert_eq!(book.chapters[0].kind, "story");
        assert_eq!(
            book.chapters[0].text,
            "Copyright\n\nAn original notice.\n\nThe Crossing\n\nMira rowed home 😀."
        );
        let chars: Vec<_> = book.chapters[0].text.chars().collect();
        let (start, end) = book.chapters[0].lines[3];
        assert_eq!(
            chars[start..end].iter().collect::<String>(),
            "Mira rowed home 😀."
        );
    }

    #[test]
    fn conflicting_whole_document_semantics_do_not_fall_back_to_a_matter_heading() {
        for semantics in ["frontmatter bodymatter", "frontmatter backmatter"] {
            let raw = format!(
                r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body epub:type="{semantics}"><h1>Copyright</h1><p>The ferry left at dawn.</p></body></html>"#
            );
            let bytes = structured_epub(
                r#"<item id="c1" href="copyright.xhtml" media-type="application/xhtml+xml"/>"#,
                r#"<spine><itemref idref="c1"/></spine>"#,
                &[("OPS/copyright.xhtml", &raw)],
            );
            let book = parse("lantern.epub", &bytes).unwrap();
            assert_eq!(book.chapters.len(), 1);
            assert_eq!(book.chapters[0].kind, "story", "{semantics}");
            assert_eq!(
                book.chapters[0].text,
                "Copyright\n\nThe ferry left at dawn."
            );
        }
    }

    #[test]
    fn coherent_matter_stays_matter_and_nested_notes_keep_their_story_unit() {
        let cases = [
            (
                r#"<body epub:type="frontmatter"><nav epub:type="toc"><a epub:type="chapter" href="elsewhere.xhtml">Unrelated story link</a></nav><section epub:type="copyright-page"><h1>Copyright</h1><p>An original notice.</p></section></body>"#,
                "front_matter",
                "Copyright\n\nAn original notice.",
            ),
            (
                r#"<body><section epub:type="chapter"><h1>The Crossing</h1><p>Mira rowed home.</p><aside epub:type="backmatter"><p>A note about lanterns.</p></aside></section></body>"#,
                "story",
                "The Crossing\n\nMira rowed home.\n\nA note about lanterns.",
            ),
        ];
        for (body, kind, text) in cases {
            let raw = format!(r#"<html xmlns:epub="http://www.idpf.org/2007/ops">{body}</html>"#);
            let bytes = structured_epub(
                r#"<item id="c1" href="one.xhtml" media-type="application/xhtml+xml"/>"#,
                r#"<spine><itemref idref="c1"/></spine>"#,
                &[("OPS/one.xhtml", &raw)],
            );
            let book = parse("lantern.epub", &bytes).unwrap();
            assert_eq!(book.chapters.len(), 1);
            assert_eq!(book.chapters[0].kind, kind);
            assert_eq!(book.chapters[0].text, text);
        }
    }

    pub fn build_epub(encryption: Option<&str>, with_cover: bool) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut z = ZipWriter::new(Cursor::new(&mut out));
            let o = SimpleFileOptions::default();
            z.start_file("mimetype", o).unwrap();
            z.write_all(b"application/epub+zip").unwrap();
            z.start_file("META-INF/container.xml", o).unwrap();
            z.write_all(br#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#).unwrap();
            if let Some(e) = encryption {
                z.start_file("META-INF/encryption.xml", o).unwrap();
                z.write_all(e.as_bytes()).unwrap();
            }
            z.start_file("OEBPS/content.opf", o).unwrap();
            let cover_meta = if with_cover {
                r#"<meta name="cover" content="cov"/>"#
            } else {
                ""
            };
            let cover_item = if with_cover {
                r#"<item id="cov" href="images/cover.png" media-type="image/png"/>"#
            } else {
                ""
            };
            z.write_all(format!(r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>The Test Ferry</dc:title><dc:creator>A. Writer</dc:creator><dc:creator>B. Editor</dc:creator>{cover_meta}</metadata><manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="c0" href="copyright.xhtml" media-type="application/xhtml+xml"/><item id="c1" href="ch%201.xhtml" media-type="application/xhtml+xml"/><item id="c2" href="ch2.xhtml" media-type="application/xhtml+xml"/>{cover_item}</manifest><spine><itemref idref="nav"/><itemref idref="c0"/><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#).as_bytes()).unwrap();
            z.start_file("OEBPS/nav.xhtml", o).unwrap();
            z.write_all(b"<html><body><nav><p>Contents</p></nav></body></html>")
                .unwrap();
            z.start_file("OEBPS/copyright.xhtml", o).unwrap();
            z.write_all(b"<html><body><h1>Copyright</h1><p>All rights reserved by nobody.</p></body></html>").unwrap();
            z.start_file("OEBPS/ch 1.xhtml", o).unwrap();
            z.write_all("<html><head><title>x</title></head><body><h1>The Crossing</h1><p>The ferry left at dusk.</p><p>\u{201C}Wait,\u{201D} she said.</p></body></html>".as_bytes()).unwrap();
            z.start_file("OEBPS/ch2.xhtml", o).unwrap();
            z.write_all(
                b"<html><body><h1>The Far Shore</h1><p>Nobody was there.</p></body></html>",
            )
            .unwrap();
            if with_cover {
                z.start_file("OEBPS/images/cover.png", o).unwrap();
                let img = image::RgbImage::from_pixel(60, 90, image::Rgb([200, 40, 40]));
                let mut png = Vec::new();
                image::DynamicImage::ImageRgb8(img)
                    .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                    .unwrap();
                z.write_all(&png).unwrap();
            }
            z.finish().unwrap();
        }
        out
    }

    #[test]
    fn reads_an_epub() {
        let b = parse("the_test_ferry.epub", &build_epub(None, true)).unwrap();
        assert_eq!(b.title, "The Test Ferry");
        assert_eq!(b.author, "A. Writer, B. Editor");
        assert!(b.cover.is_some());
        let titles: Vec<&str> = b.chapters.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Copyright", "The Crossing", "The Far Shore"],
            "nav skipped, percent-encoded href resolved"
        );
        let kinds: Vec<&str> = b.chapters.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, ["front_matter", "story", "story"]);
        assert!(b.chapters[1]
            .text
            .contains("\u{201C}Wait,\u{201D} she said."));
    }

    #[test]
    fn font_obfuscation_is_not_drm_but_anything_else_is() {
        let fonts = r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><EncryptedData xmlns="http://www.w3.org/2001/04/xmlenc#"><EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/></EncryptedData></encryption>"#;
        assert!(parse("a.epub", &build_epub(Some(fonts), false)).is_ok());
        let drm = r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><EncryptedData xmlns="http://www.w3.org/2001/04/xmlenc#"><EncryptionMethod Algorithm="http://www.w3.org/2001/04/xmlenc#aes128-cbc"/></EncryptedData></encryption>"#;
        assert_eq!(
            parse("a.epub", &build_epub(Some(drm), false)).err(),
            Some(ImportFailure::Drm)
        );
    }

    #[test]
    fn junk_and_empty_files_fail_cleanly() {
        assert!(matches!(
            parse("a.epub", b"PK not really"),
            Err(ImportFailure::Unreadable(_))
        ));
        assert_eq!(parse("a.txt", b"").err(), Some(ImportFailure::NoText));
        assert_eq!(
            parse("a.txt", b"  \n\n  ").err(),
            Some(ImportFailure::NoText)
        );
        assert!(matches!(
            parse("a.pdf", b"x"),
            Err(ImportFailure::Unreadable(_))
        ));
    }

    #[test]
    fn text_files_must_be_utf8_and_not_binary() {
        assert!(matches!(
            parse("a.txt", &[0x66, 0x6f, 0xff, 0xfe]),
            Err(ImportFailure::UnsupportedEncoding(_))
        ));
        assert!(matches!(
            parse("a.txt", b"abc\0def"),
            Err(ImportFailure::UnsupportedEncoding(_))
        ));
        let b = parse(
            "my_story_name.txt",
            "\u{FEFF}Hello there.\r\n\r\nSecond.".as_bytes(),
        )
        .unwrap();
        assert_eq!(b.title, "my story name");
        assert_eq!(b.chapters[0].text, "Hello there.\n\nSecond.");
    }

    #[test]
    fn hrefs_cannot_escape_the_archive() {
        assert_eq!(resolve("OEBPS", "../../etc/passwd"), "etc/passwd");
        assert_eq!(resolve("OEBPS", "a/../b.xhtml#frag"), "OEBPS/b.xhtml");
        assert_eq!(resolve("OEBPS", "/abs.xhtml"), "abs.xhtml");
        assert_eq!(resolve("OEBPS", "café%😀.xhtml"), "OEBPS/café%😀.xhtml");
    }
}
