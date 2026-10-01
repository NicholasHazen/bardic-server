//! Reading a book file into chapters. Pure: no database, no network.
//!
//! Limits (the contract's): EPUBs may expand to at most 100 MB and 5,000 files.
//! DRM-protected books are refused, never partially read.

use crate::text::{
    build_chapter, extract_markup, kind_from_title, paragraphs_from_plain, split_plain_chapters,
};
use std::io::{Cursor, Read};
use zip::ZipArchive;

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
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
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
        let ex = extract_markup(&String::from_utf8_lossy(&raw));
        if ex.paragraphs.is_empty() {
            continue;
        }
        text_bytes += ex.paragraphs.iter().map(String::len).sum::<usize>();
        let hint = ex
            .title
            .clone()
            .unwrap_or_else(|| idref.replace(['_', '-'], " "));
        let mut kind = kind_from_title(&hint);
        if kind == "story" {
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
        let title = match ex.title {
            Some(t) => t,
            None => {
                story_n += 1;
                format!("Section {story_n}")
            }
        };
        let (text, lines) = build_chapter(&ex.paragraphs);
        chapters.push(ParsedChapter {
            title,
            kind,
            text,
            lines,
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
    }
}
