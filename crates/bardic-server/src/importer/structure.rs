//! Optional EPUB structure metadata over the existing immutable spine units.
//!
//! Navigation names documents; it never changes reading order or splits text.
//! Broken optional XML and links are ignored so readable source still imports.

use roxmltree::{Document, Node};
use std::collections::{HashMap, HashSet};

const EPUB: &str = "http://www.idpf.org/2007/ops";

#[derive(Default)]
pub(super) struct Structure {
    navigation: Vec<Target>,
    ncx: Vec<Target>,
    landmarks: Vec<Target>,
    guide: Vec<Target>,
    pages: Vec<Target>,
}

struct Target {
    path: String,
    fragment: Option<String>,
    label: String,
    depth: usize,
    kind: Option<&'static str>,
}

impl Structure {
    /// `path` is the navigation document's archive path, not the OPF path.
    pub(super) fn add_navigation(&mut self, path: &str, raw: &str) {
        let Ok(doc) = xml(raw) else { return };
        for nav in doc.descendants().filter(|n| named(*n, "nav")) {
            let tokens = semantics(nav);
            let targets = if tokens.iter().any(|t| t == "toc") {
                &mut self.navigation
            } else if tokens.iter().any(|t| t == "landmarks") {
                &mut self.landmarks
            } else if tokens.iter().any(|t| t == "page-list" || t == "pagelist") {
                &mut self.pages
            } else {
                continue;
            };
            for link in nav.descendants().filter(|n| named(*n, "a")) {
                let Some(href) = link.attribute("href") else {
                    continue;
                };
                let Some((path, fragment)) = resolve(path, href) else {
                    continue;
                };
                let label = label(link);
                if label.is_empty() {
                    continue;
                }
                let depth = link
                    .ancestors()
                    .take_while(|n| *n != nav)
                    .filter(|n| named(*n, "ol"))
                    .count()
                    .saturating_sub(1);
                targets.push(Target {
                    path,
                    fragment,
                    label,
                    depth,
                    kind: kind_from_tokens(&semantics(link)),
                });
            }
        }
    }

    /// Legacy EPUB 2 navigation; used when a document has no valid EPUB 3 title.
    pub(super) fn add_ncx(&mut self, path: &str, raw: &str) {
        let Ok(doc) = xml(raw) else { return };
        for point in doc.descendants().filter(|n| named(*n, "navPoint")) {
            let Some(content) = point.children().find(|n| named(*n, "content")) else {
                continue;
            };
            let Some((path, fragment)) = content.attribute("src").and_then(|s| resolve(path, s))
            else {
                continue;
            };
            let title = point
                .children()
                .find(|n| named(*n, "navLabel"))
                .map(label)
                .unwrap_or_default();
            if title.is_empty() {
                continue;
            }
            self.ncx.push(Target {
                path,
                fragment,
                label: title,
                depth: point
                    .ancestors()
                    .skip(1)
                    .filter(|n| named(*n, "navPoint"))
                    .count(),
                kind: None,
            });
        }
        for point in doc.descendants().filter(|n| named(*n, "pageTarget")) {
            let Some((path, fragment)) = point
                .children()
                .find(|n| named(*n, "content"))
                .and_then(|n| n.attribute("src"))
                .and_then(|s| resolve(path, s))
            else {
                continue;
            };
            let label = point
                .children()
                .find(|n| named(*n, "navLabel"))
                .map(label)
                .unwrap_or_default();
            self.pages.push(Target {
                path,
                fragment,
                label,
                depth: 0,
                kind: None,
            });
        }
    }

    /// Legacy OPF guide references add classification, never chapter names.
    pub(super) fn add_guide(&mut self, package_path: &str, raw_opf: &str) {
        let Ok(doc) = xml(raw_opf) else { return };
        for guide in doc.descendants().filter(|n| named(*n, "guide")) {
            for reference in guide.children().filter(|n| named(*n, "reference")) {
                let Some((path, fragment)) = reference
                    .attribute("href")
                    .and_then(|s| resolve(package_path, s))
                else {
                    continue;
                };
                let tokens = reference
                    .attribute("type")
                    .unwrap_or("")
                    .split_whitespace()
                    .map(|t| t.to_ascii_lowercase())
                    .collect::<Vec<_>>();
                self.guide.push(Target {
                    path,
                    fragment,
                    label: String::new(),
                    depth: 0,
                    kind: kind_from_tokens(&tokens),
                });
            }
        }
    }

    /// Returns a navigation title and explicit structural classification.
    /// Existing heading/filename heuristics remain the importer's fallback.
    pub(super) fn chapter_metadata(
        &self,
        path: &str,
        raw: &str,
    ) -> (Option<String>, Option<&'static str>) {
        let doc = xml(raw).ok();
        let anchors: HashSet<&str> = doc
            .as_ref()
            .into_iter()
            .flat_map(|doc| doc.descendants())
            .filter_map(|node| node.attribute("id"))
            .collect();
        let valid = |target: &&Target| {
            target.path == path
                && target
                    .fragment
                    .as_deref()
                    .is_none_or(|id| anchors.contains(id))
        };
        let navigation = self.navigation.iter().filter(valid).collect::<Vec<_>>();
        let ncx = self.ncx.iter().filter(valid).collect::<Vec<_>>();
        let scopes = doc.as_ref().map(document_scopes).unwrap_or_default();
        let explicit = if doc.as_ref().is_some_and(document_has_mixed_semantics) {
            // A source unit containing story and matter cannot be skipped as a
            // whole. Preserve it as story even when its first heading names matter.
            Some("story")
        } else {
            scopes
                .iter()
                .find_map(|node| kind_from_tokens(&semantics(*node)))
        };
        let start_ids = doc.as_ref().map(document_start_ids).unwrap_or_default();
        let whole_document = |target: &&Target| {
            valid(target)
                && target
                    .fragment
                    .as_deref()
                    .is_none_or(|id| start_ids.contains(id))
        };
        // Consider all valid sibling targets before checking the chosen one's
        // start position. Dropping later targets first could make a mixed unit
        // appear to contain only one chapter.
        let chosen = choose_document_title(&navigation)
            .filter(whole_document)
            .or_else(|| choose_document_title(&ncx).filter(whole_document));
        let kind = explicit
            .or_else(|| {
                self.landmarks
                    .iter()
                    .filter(whole_document)
                    .find_map(|t| t.kind)
            })
            .or_else(|| {
                self.guide
                    .iter()
                    .filter(whole_document)
                    .find_map(|t| t.kind)
            })
            .or_else(|| chosen.filter(whole_document).and_then(|t| t.kind))
            .or_else(|| {
                // Several sibling sections share one immutable source unit.
                // Its first heading might say Copyright while later text is
                // story. Do not let that heading hide the entire mixed unit.
                let authoritative = if navigation.is_empty() {
                    &ncx
                } else {
                    &navigation
                };
                (!authoritative.is_empty() && choose_document_title(authoritative).is_none())
                    .then_some("story")
            });
        (chosen.map(|target| target.label.clone()), kind)
    }

    /// Pages are source metadata, not a reading-time/word-count estimate.
    /// Require a first source-page boundary before the first readable word and
    /// consecutive numeric page labels. A partial list, an unresolved anchor,
    /// a chapter beginning partway through a page or ambiguous labels is unknown.
    pub(super) fn page_count(&self, path: &str, raw: &str) -> Option<i64> {
        let doc = xml(raw).ok()?;
        let mut anchor_counts = HashMap::new();
        for id in doc.descendants().filter_map(|node| node.attribute("id")) {
            *anchor_counts.entry(id).or_insert(0usize) += 1;
        }
        let mut linked = HashMap::new();
        for page in self.pages.iter().filter(|page| page.path == path) {
            let id = page.fragment.as_deref()?;
            if anchor_counts.get(id) != Some(&1) {
                return None;
            }
            let number = page_number(&page.label)?;
            if linked.insert(id, number).is_some_and(|old| old != number) {
                return None;
            }
        }
        let body = doc
            .root_element()
            .descendants()
            .find(|node| named(*node, "body"))
            .unwrap_or(doc.root_element());
        let mut pending = vec![(body, false)];
        let mut seen_text = false;
        let mut previous = None;
        let mut count = 0;
        let mut page_has_text = false;
        while let Some((node, inside_page_marker)) = pending.pop() {
            if node.is_element()
                && ["script", "style", "head", "nav", "svg"]
                    .iter()
                    .any(|name| named(node, name))
            {
                continue;
            }
            let mut is_page_marker = inside_page_marker;
            if node.is_element() {
                let from_list = node.attribute("id").and_then(|id| linked.remove(id));
                let marked = semantics(node).iter().any(|token| token == "pagebreak");
                is_page_marker |= marked;
                let from_marker = if marked {
                    Some(page_number(
                        node.attribute("aria-label")
                            .or_else(|| node.attribute("title"))
                            .map(str::to_string)
                            .unwrap_or_else(|| label(node))
                            .as_str(),
                    ))
                } else {
                    None
                };
                let number = match (from_list, from_marker) {
                    (Some(list), Some(Some(marker))) if list != marker => return None,
                    (Some(list), _) => Some(list),
                    (None, Some(marker)) => Some(marker?),
                    (None, None) => None,
                };
                if let Some(number) = number {
                    if previous.is_none() && seen_text
                        || previous.is_some_and(|last: i64| last.checked_add(1) != Some(number))
                    {
                        return None;
                    }
                    previous = Some(number);
                    page_has_text = false;
                }
            }
            if node.is_text()
                && !inside_page_marker
                && node.text().is_some_and(|text| !text.trim().is_empty())
            {
                seen_text = true;
                if previous.is_some() && !page_has_text {
                    count += 1;
                    page_has_text = true;
                }
            }
            pending.extend(node.children().rev().map(|child| (child, is_page_marker)));
        }
        (linked.is_empty() && count > 0).then_some(count)
    }
}

fn page_number(label: &str) -> Option<i64> {
    let label = label.trim();
    let label = label
        .strip_prefix("Page ")
        .or_else(|| label.strip_prefix("page "))
        .unwrap_or(label)
        .trim();
    if label.is_empty() || !label.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    label.parse::<i64>().ok().filter(|number| *number > 0)
}

fn xml(raw: &str) -> Result<Document<'_>, roxmltree::Error> {
    Document::parse_with_options(
        raw,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
}

fn named(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name().eq_ignore_ascii_case(name)
}

fn label(node: Node<'_, '_>) -> String {
    node.descendants()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn semantics(node: Node<'_, '_>) -> Vec<String> {
    node.attribute((EPUB, "type"))
        .unwrap_or("")
        .split_whitespace()
        .map(|token| token.to_ascii_lowercase())
        .chain(
            node.attribute("role")
                .unwrap_or("")
                .split_whitespace()
                .filter_map(|token| token.strip_prefix("doc-"))
                .map(|token| token.to_ascii_lowercase()),
        )
        .collect()
}

fn kind_from_tokens(tokens: &[String]) -> Option<&'static str> {
    let mut found = None;
    for token in tokens {
        let kind = match token.as_str() {
            "bodymatter" | "chapter" | "prologue" | "epilogue" | "text" => "story",
            "frontmatter" | "cover" | "titlepage" | "title-page" | "halftitlepage"
            | "copyright-page" | "copyright" | "dedication" | "epigraph" | "toc" | "foreword"
            | "preface" => "front_matter",
            "backmatter" | "acknowledgments" | "acknowledgements" | "afterword" | "appendix"
            | "bibliography" | "index" | "glossary" | "colophon" => "back_matter",
            _ => continue,
        };
        if found.is_some_and(|previous| previous != kind) {
            // Explicitly keep a conflicting unit eligible. Returning no kind
            // would let a matter heading or filename exclude it in the fallback.
            return Some("story");
        }
        found = Some(kind);
    }
    found
}

/// Typed story and matter sections may share one immutable source unit, even
/// without a TOC. Ignore metadata and other text the reader does not render.
fn document_has_mixed_semantics(doc: &Document<'_>) -> bool {
    let mut story = false;
    let mut matter = false;
    let mut pending = vec![doc.root_element()];
    while let Some(node) = pending.pop() {
        if !node.is_element()
            || ["script", "style", "head", "nav", "svg"]
                .iter()
                .any(|name| named(node, name))
        {
            continue;
        }
        match kind_from_tokens(&semantics(node)) {
            Some("story") => story = true,
            Some(_) => matter = true,
            None => {}
        }
        if story && matter {
            return true;
        }
        pending.extend(node.children());
    }
    false
}

/// Body semantics come first. A sole wrapper can describe the whole source
/// unit; a nested matter block in a mixed story document cannot.
fn document_scopes<'a, 'input>(doc: &'a Document<'input>) -> Vec<Node<'a, 'input>> {
    let root = doc.root_element();
    let body = root
        .descendants()
        .find(|node| named(*node, "body"))
        .unwrap_or(root);
    let mut scopes = vec![body];
    if root != body {
        scopes.push(root);
    }
    let mut container = body;
    loop {
        if container
            .children()
            .any(|node| node.is_text() && node.text().is_some_and(|text| !text.trim().is_empty()))
        {
            break;
        }
        let mut children = container.children().filter(|node| {
            node.is_element()
                && !["script", "style", "head", "nav", "svg"]
                    .iter()
                    .any(|name| named(*node, name))
        });
        let Some(only) = children.next() else { break };
        if children.next().is_some() {
            break;
        }
        scopes.push(only);
        container = only;
    }
    scopes
}

/// IDs of elements anchored before the first readable word, including an empty
/// leading anchor or the first heading. They can name the whole source unit;
/// an arbitrary middle-of-story note or appendix anchor cannot.
fn document_start_ids<'a, 'input>(doc: &'a Document<'input>) -> HashSet<&'a str> {
    let root = doc.root_element();
    let body = root
        .descendants()
        .find(|node| named(*node, "body"))
        .unwrap_or(root);
    let mut ids = HashSet::new();
    if let Some(id) = root.attribute("id") {
        ids.insert(id);
    }
    // An EPUB is untrusted input; deeply nested markup must not recurse on
    // the process stack. Visit in document order and stop at the first word.
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        if node.is_element()
            && ["script", "style", "head", "nav", "svg"]
                .iter()
                .any(|name| named(node, name))
        {
            continue;
        }
        if let Some(id) = node.attribute("id") {
            ids.insert(id);
        }
        if node.is_text() && node.text().is_some_and(|text| !text.trim().is_empty()) {
            break;
        }
        pending.extend(node.children().rev());
    }
    ids
}

/// Prefer a link naming the entire document. Otherwise only a unique target at
/// the shallowest TOC level can name it; sibling chapters in one file are
/// ambiguous and keep the existing source-unit heading/fallback.
fn choose_document_title<'a>(targets: &[&'a Target]) -> Option<&'a Target> {
    let whole = targets
        .iter()
        .filter(|t| t.fragment.is_none())
        .min_by_key(|t| t.depth);
    if let Some(target) = whole {
        return Some(target);
    }
    let depth = targets.iter().map(|t| t.depth).min()?;
    let top = targets
        .iter()
        .copied()
        .filter(|target| target.depth == depth)
        .collect::<Vec<_>>();
    let first = *top.first()?;
    top.iter()
        .all(|target| target.fragment == first.fragment)
        .then_some(first)
}

/// Local URL resolution; unlike filesystem canonicalization it never reads a
/// path. Decode into bytes first so non-ASCII and malformed UTF-8 cannot panic.
fn resolve(base_path: &str, href: &str) -> Option<(String, Option<String>)> {
    let (path_query, fragment) = href.trim().split_once('#').unwrap_or((href.trim(), ""));
    let path = percent_decode(path_query.split('?').next().unwrap_or(""))?;
    let fragment = percent_decode(fragment)?;
    if path.starts_with('/') || path.contains(['\\', ':', '\0']) || fragment.contains('\0') {
        return None;
    }
    let mut parts = base_path.split('/').collect::<Vec<_>>();
    if !path.is_empty() {
        parts.pop();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                part => parts.push(part),
            }
        }
    }
    Some((parts.join("/"), (!fragment.is_empty()).then_some(fragment)))
}

fn percent_decode(value: &str) -> Option<String> {
    let input = value.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let first = (*input.get(i + 1)? as char).to_digit(16)?;
            let second = (*input.get(i + 2)? as char).to_digit(16)?;
            output.push((first * 16 + second) as u8);
            i += 3;
        } else {
            output.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8(output).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(body: &str) -> String {
        format!(r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body>{body}</body></html>"#)
    }

    #[test]
    fn page_lists_count_consecutive_source_pages_without_naming_chapters() {
        let mut s = Structure::default();
        s.add_navigation("OPS/nav.xhtml", r#"<nav role="doc-pagelist"><a href="one.xhtml#p10">10</a><a href="one.xhtml#p11">11</a><a href="one.xhtml#p11">11</a></nav>"#);
        // The role spelling for EPUB pagination is doc-pagelist; EPUB type
        // page-list is also supported below.
        s.add_navigation("OPS/nav.xhtml", r#"<nav xmlns:epub="http://www.idpf.org/2007/ops" epub:type="page-list"><a href="one.xhtml#p10">10</a><a href="one.xhtml#p11">11</a></nav>"#);
        let raw = chapter(
            r#"<span id="p10"/><h1>Arrival</h1><p>The ferry rang.</p><span id="p11"/><p>The crew woke.</p>"#,
        );
        assert_eq!(s.page_count("OPS/one.xhtml", &raw), Some(2));
        assert_eq!(s.chapter_metadata("OPS/one.xhtml", &raw).0, None);
    }

    #[test]
    fn pagebreaks_and_ncx_pages_are_source_metadata() {
        let raw = chapter(
            r#"<span id="p10" epub:type="pagebreak" title="10"/><h1>Arrival</h1><span id="p11" role="doc-pagebreak" aria-label="Page 11"/><p>Words.</p>"#,
        );
        assert_eq!(Structure::default().page_count("one.xhtml", &raw), Some(2));
        let mut ncx = Structure::default();
        ncx.add_ncx("OPS/toc.ncx", r#"<ncx><pageList><pageTarget><navLabel><text>10</text></navLabel><content src="one.xhtml#p10"/></pageTarget><pageTarget><navLabel><text>11</text></navLabel><content src="one.xhtml#p11"/></pageTarget></pageList></ncx>"#);
        assert_eq!(ncx.page_count("OPS/one.xhtml", &raw), Some(2));
    }

    #[test]
    fn empty_source_page_boundaries_do_not_add_pages_to_chapter_text() {
        let s = Structure::default();
        for body in [
            r#"<span role="doc-pagebreak" title="10"/><p>Words.</p><span role="doc-pagebreak" title="11"/>"#,
            r#"<span role="doc-pagebreak" title="9"/><span role="doc-pagebreak" title="10"/><p>Words.</p>"#,
            r#"<span role="doc-pagebreak" title="9">9</span><span role="doc-pagebreak" title="10">10</span><p>Words.</p>"#,
        ] {
            assert_eq!(s.page_count("one.xhtml", &chapter(body)), Some(1), "{body}");
        }
        assert_eq!(
            s.page_count(
                "one.xhtml",
                &chapter(r#"<span role="doc-pagebreak" title="9">9</span>"#)
            ),
            None
        );
    }

    #[test]
    fn incomplete_or_ambiguous_source_pagination_is_unknown() {
        let s = Structure::default();
        for body in [
            "<p>No page metadata.</p>",
            r#"<p>Already reading.</p><span role="doc-pagebreak" title="10"/>"#,
            r#"<span role="doc-pagebreak" title="10"/><p>Words.</p><span role="doc-pagebreak" title="12"/>"#,
            r#"<span role="doc-pagebreak" title="10"/><p>Words.</p><span role="doc-pagebreak" title="10"/>"#,
            r#"<span role="doc-pagebreak" title="iv"/><p>Words.</p>"#,
            r#"<span role="doc-pagebreak"/><p>Words.</p>"#,
            r#"<head><span role="doc-pagebreak" title="10"/></head><p>Words.</p>"#,
        ] {
            assert_eq!(s.page_count("one.xhtml", &chapter(body)), None, "{body}");
        }
        for links in [
            r#"<a href="one.xhtml#missing">10</a>"#,
            r#"<a href="one.xhtml">10</a>"#,
            r#"<a href="one.xhtml#p10">10</a><a href="one.xhtml#p10">11</a>"#,
        ] {
            let mut s = Structure::default();
            s.add_navigation("nav.xhtml", &format!(r#"<nav xmlns:epub="http://www.idpf.org/2007/ops" epub:type="page-list">{links}</nav>"#));
            assert_eq!(
                s.page_count("one.xhtml", &chapter(r#"<span id="p10"/><p>Words.</p>"#)),
                None
            );
        }
        let mut s = Structure::default();
        s.add_navigation("nav.xhtml", r#"<nav xmlns:epub="http://www.idpf.org/2007/ops" epub:type="page-list"><a href="one.xhtml#p10">10</a></nav>"#);
        assert_eq!(
            s.page_count(
                "one.xhtml",
                &chapter(r#"<span id="p10"/><p>Words.</p><span id="p10"/>"#)
            ),
            None
        );
    }

    #[test]
    fn epub3_toc_names_chapters_and_page_lists_do_not() {
        let mut s = Structure::default();
        s.add_navigation("OPS/nav/nav.xhtml", r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="page-list"><a href="../text/one.xhtml">25</a></nav><nav epub:type="toc"><ol><li><a href="../text/one.xhtml#start">The <em>Crossing</em></a></li></ol></nav></body></html>"#);
        assert_eq!(
            s.chapter_metadata(
                "OPS/text/one.xhtml",
                &chapter(r#"<p id="start">Words.</p>"#)
            ),
            (Some("The Crossing".into()), None)
        );
    }

    #[test]
    fn ncx_fills_missing_or_broken_epub3_navigation() {
        let mut s = Structure::default();
        s.add_navigation("OPS/nav.xhtml", "<nav>");
        s.add_navigation(
            "OPS/nav.xhtml",
            r#"<nav role="doc-toc"><a href="one.xhtml#absent">Wrong</a></nav>"#,
        );
        s.add_ncx("OPS/toc.ncx", r#"<ncx><navMap><navPoint><navLabel><text>The Far Shore</text></navLabel><content src="one.xhtml"/></navPoint></navMap></ncx>"#);
        assert_eq!(
            s.chapter_metadata("OPS/one.xhtml", &chapter("<p>Words.</p>")),
            (Some("The Far Shore".into()), None)
        );
    }

    #[test]
    fn valid_navigation_precedes_ncx() {
        let mut s = Structure::default();
        s.add_navigation(
            "OPS/nav.xhtml",
            r#"<nav role="doc-toc"><a href="one.xhtml">Chapter One</a></nav>"#,
        );
        s.add_ncx("OPS/toc.ncx", r#"<ncx><navPoint><navLabel><text>Old title</text></navLabel><content src="one.xhtml"/></navPoint></ncx>"#);
        assert_eq!(
            s.chapter_metadata("OPS/one.xhtml", &chapter("<p>Words.</p>"))
                .0,
            Some("Chapter One".into())
        );
    }

    #[test]
    fn explicit_document_semantics_precede_landmarks_and_guide() {
        let mut s = Structure::default();
        s.add_navigation("OPS/nav.xhtml", r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><nav epub:type="landmarks"><a epub:type="frontmatter" href="one.xhtml">Start here</a></nav></html>"#);
        s.add_guide(
            "OPS/book.opf",
            r#"<package><guide><reference type="toc" href="one.xhtml"/></guide></package>"#,
        );
        assert_eq!(
            s.chapter_metadata(
                "OPS/one.xhtml",
                &chapter(r#"<section epub:type="chapter"><p>Story.</p></section>"#)
            ),
            (None, Some("story"))
        );
        assert_eq!(
            s.chapter_metadata(
                "OPS/one.xhtml",
                &chapter(r#"<section role="doc-afterword"><p>End.</p></section>"#)
            )
            .1,
            Some("back_matter")
        );
    }

    #[test]
    fn guide_and_landmarks_classify_without_inventing_titles() {
        let mut s = Structure::default();
        s.add_guide("OPS/book.opf", r#"<package><guide><reference type="title-page" href="text/title.xhtml"/><reference type="text" href="text/one.xhtml"/></guide></package>"#);
        s.add_navigation("OPS/navigation/nav.xhtml", r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><nav epub:type="landmarks"><a epub:type="bibliography" href="../text/end.xhtml">References</a></nav></html>"#);
        for (path, expected) in [
            ("OPS/text/title.xhtml", "front_matter"),
            ("OPS/text/one.xhtml", "story"),
            ("OPS/text/end.xhtml", "back_matter"),
        ] {
            assert_eq!(
                s.chapter_metadata(path, &chapter("<p>Words.</p>")),
                (None, Some(expected))
            );
        }
    }

    #[test]
    fn nested_matter_and_notes_do_not_classify_a_mixed_story_document() {
        let s = Structure::default();
        assert_eq!(
            s.chapter_metadata("one.xhtml", &chapter(r#"<div><section epub:type="chapter"><p>Story.</p></section><aside epub:type="backmatter"><p>Notes.</p></aside></div>"#)).1,
            Some("story")
        );
        assert_eq!(
            s.chapter_metadata("one.xhtml", &chapter(r#"<section epub:type="chapter"><p>Story.</p><aside epub:type="endnotes"><p>Notes.</p></aside></section>"#)).1,
            Some("story")
        );
    }

    #[test]
    fn nested_toc_uses_document_parent_but_sibling_fragments_are_ambiguous() {
        let mut s = Structure::default();
        s.add_navigation("OPS/nav.xhtml", r#"<nav role="doc-toc"><ol><li><a href="one.xhtml">Part One</a><ol><li><a href="one.xhtml#a">Chapter One</a></li><li><a href="one.xhtml#b">Chapter Two</a></li></ol></li></ol></nav>"#);
        let raw = chapter(r#"<p id="a">First.</p><p id="b">Second.</p>"#);
        assert_eq!(
            s.chapter_metadata("OPS/one.xhtml", &raw).0,
            Some("Part One".into())
        );
        let mut ambiguous = Structure::default();
        ambiguous.add_navigation("OPS/nav.xhtml", r#"<nav role="doc-toc"><ol><li><a href="one.xhtml#a">Chapter One</a></li><li><a href="one.xhtml#b">Chapter Two</a></li></ol></nav>"#);
        assert_eq!(ambiguous.chapter_metadata("OPS/one.xhtml", &raw).0, None);
    }

    #[test]
    fn fragment_landmark_inside_a_story_cannot_skip_the_whole_document() {
        let mut s = Structure::default();
        s.add_navigation("OPS/nav.xhtml", r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><nav epub:type="landmarks"><a epub:type="backmatter" href="one.xhtml#notes">Notes</a></nav></html>"#);
        assert_eq!(
            s.chapter_metadata(
                "OPS/one.xhtml",
                &chapter(r#"<p>Story.</p><aside id="notes"><p>Notes.</p></aside>"#)
            )
            .1,
            None
        );
        s.add_navigation(
            "OPS/nav.xhtml",
            r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><nav epub:type="toc"><a epub:type="backmatter" href="one.xhtml#notes">Notes</a></nav></html>"#,
        );
        assert_eq!(
            s.chapter_metadata(
                "OPS/one.xhtml",
                &chapter(r#"<p>Story.</p><aside id="notes"><p>Notes.</p></aside>"#)
            )
            .1,
            None
        );
        assert_eq!(
            s.chapter_metadata(
                "OPS/one.xhtml",
                &chapter(r#"<p>Story.</p><aside id="notes"><p>Notes.</p></aside>"#)
            )
            .0,
            None
        );
    }

    #[test]
    fn first_heading_and_empty_leading_anchor_can_name_the_document() {
        let mut s = Structure::default();
        s.add_navigation(
            "OPS/nav.xhtml",
            r#"<nav role="doc-toc"><a href="one.xhtml#start">The Crossing</a></nav>"#,
        );
        for body in [
            r#"<h1 id="start">Old heading</h1><p>Words.</p>"#,
            r#"<a id="start"/><h1>Old heading</h1><p>Words.</p>"#,
            r#"<div><a id="start"/><p>Words.</p></div>"#,
            r#"<script>Ignored words.</script><h1 id="start">Old heading</h1><p>Words.</p>"#,
        ] {
            assert_eq!(
                s.chapter_metadata("OPS/one.xhtml", &chapter(body)).0,
                Some("The Crossing".into()),
                "{body}"
            );
        }
    }

    #[test]
    fn middle_fragment_title_is_ignored_and_ncx_start_title_can_fill_it() {
        let mut s = Structure::default();
        s.add_navigation("OPS/nav.xhtml", r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><nav epub:type="toc"><a epub:type="backmatter" href="one.xhtml#notes">Acknowledgements</a></nav></html>"#);
        let raw = chapter(
            r#"<h1 id="start">Story</h1><p>Words.</p><aside id="notes"><p>Notes.</p></aside>"#,
        );
        assert_eq!(s.chapter_metadata("OPS/one.xhtml", &raw), (None, None));
        s.add_ncx("OPS/toc.ncx", r#"<ncx><navPoint><navLabel><text>Chapter One</text></navLabel><content src="one.xhtml#start"/></navPoint></ncx>"#);
        assert_eq!(
            s.chapter_metadata("OPS/one.xhtml", &raw),
            (Some("Chapter One".into()), None)
        );
    }

    #[test]
    fn local_resolution_decodes_unicode_and_rejects_external_or_unsafe_links() {
        assert_eq!(
            resolve("OPS/nav/toc.xhtml", "../text/caf%C3%A9.xhtml#d%C3%A9but"),
            Some(("OPS/text/café.xhtml".into(), Some("début".into())))
        );
        for href in [
            "https://example.com/one.xhtml",
            "//example.com/one.xhtml",
            "../../../escape.xhtml",
            "../text/a%FF.xhtml",
            "../text/a%.xhtml",
            "../text/a%00.xhtml",
            "..%5Cescape.xhtml",
        ] {
            assert_eq!(resolve("OPS/nav/toc.xhtml", href), None, "{href}");
        }
        let mut s = Structure::default();
        s.add_navigation(
            "OPS/nav.xhtml",
            r#"<nav role="doc-toc"><a href="caf%C3%A9.xhtml#d%C3%A9but">Café</a></nav>"#,
        );
        assert_eq!(
            s.chapter_metadata("OPS/café.xhtml", &chapter(r#"<p id="début">Words.</p>"#))
                .0,
            Some("Café".into())
        );
    }

    #[test]
    fn malformed_chapter_can_still_use_a_document_link() {
        let mut s = Structure::default();
        s.add_navigation(
            "OPS/nav.xhtml",
            r#"<nav role="doc-toc"><a href="one.xhtml">Arrival</a></nav>"#,
        );
        assert_eq!(
            s.chapter_metadata("OPS/one.xhtml", "<p>Words."),
            (Some("Arrival".into()), None)
        );
        assert_eq!(
            Structure::default().chapter_metadata("one.xhtml", "<body>"),
            (None, None)
        );
    }
}
