//! One note's text, parsed into what the index needs: frontmatter
//! properties, links, tags, headings, block ids and tasks.
//!
//! The split of labour is deliberate. pulldown-cmark knows CommonMark, so it
//! decides what is code (fenced, indented, inline), what is math, where the
//! headings are and where the inline `[text](dest)` links are. Everything
//! Obsidian adds on top — `[[wikilinks]]`, `![[embeds]]`, `#tags`, `^block`
//! ids, `%%comments%%`, task statuses other than `x` — is scanned from the
//! raw bytes by hand, skipping the ranges pulldown called code. Scanning raw
//! text rather than pulldown's `Text` events is what gives every link an
//! exact byte span, which the rename path needs to rewrite a link in place
//! without touching the bytes around it; `Text` events split at arbitrary
//! punctuation and never carry the brackets.
//!
//! pulldown's own `ENABLE_WIKILINKS` stays off: it would turn `[[x]]` into
//! link events we would then have to reconcile with the scanner, and it has
//! no notion of `![[embed]]` or `\|` inside tables.

use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Frontmatter properties, in file order. An `IndexMap` rather than
/// `serde_json`'s `preserve_order` feature, which would unify across the
/// workspace and reorder every other crate's JSON (see `canvas`). Maps
/// nested inside a value are ordinary sorted `serde_json` maps.
pub type Properties = indexmap::IndexMap<String, Value>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub properties: Properties,
    /// Byte range of the whole frontmatter block, delimiters included.
    pub frontmatter: Option<Range<usize>>,
    pub links: Vec<Link>,
    pub tags: Vec<NoteTag>,
    pub headings: Vec<Heading>,
    pub blocks: Vec<BlockId>,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// `[[target#sub|display]]`, or `![[...]]` when `embed`.
    Wiki,
    /// `[display](target)`, or `![alt](target)` when `embed`.
    Markdown,
    /// A canvas `file` node. Spans are empty: the rewrite edits the node's
    /// `file` field instead (see `canvas`).
    CanvasFile,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub kind: LinkKind,
    pub embed: bool,
    /// The link path with any `#heading` / `#^block` subpath removed.
    /// Markdown targets are percent-decoded.
    pub target: String,
    /// Heading or block reference, without the leading `#`
    /// (`Heading`, `^block-id`).
    pub subpath: Option<String>,
    pub display: Option<String>,
    /// The whole link, from `!` or the first bracket to the last.
    pub span: Range<usize>,
    /// The bytes of the link path alone, as written (still encoded for a
    /// markdown link). A rename replaces exactly these bytes.
    pub target_span: Range<usize>,
    /// 0-based line of `span.start`.
    pub line: usize,
    /// The canvas node the link came from; `None` in a Markdown note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// A tag, without its `#`. (`Tag` is pulldown's name.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteTag {
    pub name: String,
    /// `None` for a tag that came from the `tags` property.
    pub line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockId {
    pub id: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// The character between the brackets: ' ' open, 'x' done, and the
    /// custom statuses themes and the Tasks plugin use ('/', '-', '>', …).
    pub status: char,
    pub text: String,
    pub line: usize,
    /// Byte offset of the status character, for toggling in place.
    pub status_at: usize,
}

impl Task {
    /// Open means still to do: a blank box, or `/` (in progress) as the
    /// Tasks plugin uses it. Everything else — `x`, `-` cancelled,
    /// `>` forwarded — is closed.
    pub fn is_open(&self) -> bool {
        matches!(self.status, ' ' | '/')
    }
}

/// Parse a note's full text.
pub fn parse(src: &str) -> Note {
    let lines = LineIndex::new(src);
    let mut note = Note::default();

    let body_start = match frontmatter_range(src) {
        Some((whole, inner)) => {
            note.properties = parse_properties(&src[inner]);
            note.frontmatter = Some(whole.clone());
            whole.end
        }
        None => 0,
    };
    for name in property_tags(&note.properties) {
        note.tags.push(NoteTag { name, line: None });
    }

    // Everything code-like is invisible to the Obsidian scanners.
    let mut excluded: Vec<Range<usize>> = Vec::new();
    let mut md_links: Vec<Link> = Vec::new();
    let mut heading: Option<(u8, usize, String)> = None;

    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    for (event, range) in Parser::new_ext(src, opts).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => excluded.push(range),
            Event::Code(text) => {
                excluded.push(range);
                if let Some((_, _, buf)) = heading.as_mut() {
                    buf.push_str(&text);
                }
            }
            Event::InlineMath(_) | Event::DisplayMath(_) => excluded.push(range),
            Event::Start(Tag::Heading { level, .. }) => {
                heading = Some((level as u8, range.start, String::new()))
            }
            Event::Text(text) => {
                if let Some((_, _, buf)) = heading.as_mut() {
                    buf.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, start, text)) = heading.take() {
                    note.headings.push(Heading {
                        level,
                        text: text.trim().to_string(),
                        line: lines.line_of(start),
                    });
                }
            }
            Event::Start(Tag::Link { link_type: pulldown_cmark::LinkType::Inline, .. }) => {
                if let Some(link) = markdown_link(src, range, false, &lines) {
                    md_links.push(link);
                }
            }
            Event::Start(Tag::Image { link_type: pulldown_cmark::LinkType::Inline, .. }) => {
                if let Some(link) = markdown_link(src, range, true, &lines) {
                    md_links.push(link);
                }
            }
            _ => {}
        }
    }
    excluded.sort_by_key(|r| r.start);
    let comments = comment_ranges(src, body_start, &excluded);
    excluded.extend(comments.iter().cloned());
    excluded.sort_by_key(|r| r.start);
    let skip = Ranges(excluded);

    // Headings pulldown found inside a %%comment%% are not headings, and
    // neither is anything it read in a frontmatter block it did not
    // recognise as one (`---` can also be a rule or a setext underline).
    let comment_skip = Ranges(comments);
    note.headings.retain(|h| {
        let at = lines.start_of(h.line);
        at >= body_start && !comment_skip.contains(at)
    });

    // Wikilinks are scanned over the frontmatter too: Obsidian indexes a
    // `related: "[[Other]]"` property as a link.
    let wikis = wikilinks(src, &skip, &lines);
    let mut links: Vec<Link> = wikis;
    links.extend(md_links.into_iter().filter(|l| !skip.contains(l.span.start)));
    links.sort_by_key(|l| l.span.start);
    note.links = links;

    let link_spans = Ranges(note.links.iter().map(|l| l.span.clone()).collect());
    scan_tags(src, body_start, &skip, &link_spans, &lines, &mut note.tags);
    scan_lines(src, body_start, &skip, &lines, &mut note);
    note
}

/// `---\n...\n---` at the very top of the file: (whole block, inner YAML).
pub fn frontmatter_range(src: &str) -> Option<(Range<usize>, Range<usize>)> {
    let first_end = src.find('\n')?;
    if src[..first_end].trim_end_matches('\r') != "---" {
        return None;
    }
    let inner_start = first_end + 1;
    let mut pos = inner_start;
    while pos <= src.len() {
        let end = src[pos..].find('\n').map(|i| pos + i).unwrap_or(src.len());
        if src[pos..end].trim_end_matches('\r') == "---" {
            let whole_end = if end < src.len() { end + 1 } else { end };
            return Some((0..whole_end, inner_start..pos));
        }
        if end == src.len() {
            break;
        }
        pos = end + 1;
    }
    None
}

/// YAML → JSON values. A block that does not parse, or is not a mapping,
/// yields no properties rather than an error: Obsidian shows such a note
/// with its frontmatter as plain text, and the index should still hold it.
pub fn parse_properties(yaml: &str) -> Properties {
    let docs = match yaml_rust2::YamlLoader::load_from_str(yaml) {
        Ok(docs) => docs,
        Err(_) => return Properties::new(),
    };
    match docs.into_iter().next() {
        Some(yaml_rust2::Yaml::Hash(h)) => h
            .into_iter()
            .filter_map(|(k, v)| Some((yaml_key(k)?, yaml_to_json(v))))
            .collect(),
        _ => Properties::new(),
    }
}

fn yaml_key(k: yaml_rust2::Yaml) -> Option<String> {
    use yaml_rust2::Yaml;
    match k {
        Yaml::String(s) | Yaml::Real(s) => Some(s),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn yaml_to_json(y: yaml_rust2::Yaml) -> Value {
    use yaml_rust2::Yaml;
    match y {
        Yaml::Real(s) => s
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::String(s)),
        Yaml::Integer(i) => Value::from(i),
        Yaml::String(s) => Value::String(s),
        Yaml::Boolean(b) => Value::Bool(b),
        Yaml::Array(a) => Value::Array(a.into_iter().map(yaml_to_json).collect()),
        Yaml::Hash(h) => Value::Object(
            h.into_iter().filter_map(|(k, v)| Some((yaml_key(k)?, yaml_to_json(v)))).collect::<Map<_, _>>(),
        ),
        Yaml::Null | Yaml::Alias(_) | Yaml::BadValue => Value::Null,
    }
}

/// `tags` (or the older `tag`) as a list, or as one string split on commas
/// and whitespace. A leading `#` is tolerated and dropped.
fn property_tags(props: &Properties) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["tags", "tag"] {
        let push = |s: &str, out: &mut Vec<String>| {
            for part in s.split(|c: char| c == ',' || c.is_whitespace()) {
                let t = part.trim().trim_start_matches('#');
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
        };
        match props.get(key) {
            Some(Value::String(s)) => push(s, &mut out),
            Some(Value::Array(items)) => {
                for item in items {
                    match item {
                        Value::String(s) => push(s, &mut out),
                        Value::Number(n) => out.push(n.to_string()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Aliases from the `aliases` (or `alias`) property.
pub fn aliases(props: &Properties) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["aliases", "alias"] {
        match props.get(key) {
            Some(Value::String(s)) => {
                out.extend(s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from))
            }
            Some(Value::Array(items)) => {
                out.extend(items.iter().filter_map(Value::as_str).map(String::from))
            }
            _ => {}
        }
    }
    out
}

/// Byte ranges sorted by start, with a point query.
struct Ranges(Vec<Range<usize>>);

impl Ranges {
    fn contains(&self, pos: usize) -> bool {
        // Ranges can nest (a code span inside a comment), so any range that
        // starts at or before `pos` may hold it. A note has a few dozen.
        let idx = self.0.partition_point(|r| r.start <= pos);
        self.0[..idx].iter().any(|r| r.contains(&pos))
    }
}

/// Byte offset → 0-based line.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(src: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { starts }
    }
    pub fn line_of(&self, pos: usize) -> usize {
        self.starts.partition_point(|&s| s <= pos) - 1
    }
    pub fn start_of(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(0)
    }
}

/// `%%...%%` comments outside code; an unclosed one runs to the end of the
/// file, as Obsidian renders it.
fn comment_ranges(src: &str, from: usize, code: &[Range<usize>]) -> Vec<Range<usize>> {
    let code = Ranges(code.to_vec());
    let mut out = Vec::new();
    let mut pos = from;
    while let Some(i) = src[pos..].find("%%") {
        let start = pos + i;
        if code.contains(start) {
            pos = start + 2;
            continue;
        }
        let end = src[start + 2..].find("%%").map(|j| start + 2 + j + 2).unwrap_or(src.len());
        out.push(start..end);
        pos = end;
    }
    out
}

fn wikilinks(src: &str, skip: &Ranges, lines: &LineIndex) -> Vec<Link> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = src[pos..].find("[[") {
        let open = pos + i;
        pos = open + 2;
        if skip.contains(open) || (open > 0 && bytes[open - 1] == b'\\') {
            continue;
        }
        // The link ends at the first `]]` on the same line.
        let rest = &src[open + 2..];
        let line_end = rest.find('\n').unwrap_or(rest.len());
        let Some(close_rel) = rest[..line_end].find("]]") else { continue };
        let inner_start = open + 2;
        let inner = &src[inner_start..inner_start + close_rel];
        let close = inner_start + close_rel + 2;
        let embed = open > 0 && bytes[open - 1] == b'!';
        let span_start = if embed { open - 1 } else { open };

        // Inside a table the pipe is escaped as `\|`; the path then ends
        // before the backslash.
        let (path_part, display) = match inner.find('|') {
            Some(p) => {
                let path_end = if p > 0 && inner.as_bytes()[p - 1] == b'\\' { p - 1 } else { p };
                (&inner[..path_end], Some(inner[p + 1..].to_string()))
            }
            None => (inner, None),
        };
        let (target_raw, subpath) = match path_part.find('#') {
            Some(h) => (&path_part[..h], Some(path_part[h + 1..].trim().to_string())),
            None => (path_part, None),
        };
        let lead = target_raw.len() - target_raw.trim_start().len();
        let target = target_raw.trim();
        if target.is_empty() {
            // `[[#Heading]]` points into the note itself; nothing to index.
            pos = close;
            continue;
        }
        let t_start = inner_start + lead;
        out.push(Link {
            kind: LinkKind::Wiki,
            embed,
            target: target.to_string(),
            subpath: subpath.filter(|s| !s.is_empty()),
            display,
            span: span_start..close,
            target_span: t_start..t_start + target.len(),
            line: lines.line_of(span_start),
            node: None,
        });
        pos = close;
    }
    out
}

/// A `[text](dest)` link from pulldown's range, with the destination's own
/// byte span found by re-reading the source: pulldown hands back the
/// destination unescaped and gives no offset for it.
fn markdown_link(src: &str, range: Range<usize>, embed: bool, lines: &LineIndex) -> Option<Link> {
    let text = &src[range.clone()];
    let bytes = text.as_bytes();
    let mut i = if embed { 1 } else { 0 };
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    // Find the `]` closing the link text, honouring nesting and escapes.
    let mut depth = 0i32;
    let close_text = loop {
        match bytes.get(i)? {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break i;
                }
            }
            _ => {}
        }
        i += 1;
    };
    if bytes.get(close_text + 1) != Some(&b'(') {
        return None;
    }
    let mut d = close_text + 2;
    while bytes.get(d).is_some_and(|b| *b == b' ' || *b == b'\t' || *b == b'\n') {
        d += 1;
    }
    let (dest_start, dest_end) = if bytes.get(d) == Some(&b'<') {
        let end = text[d + 1..].find('>')? + d + 1;
        (d + 1, end)
    } else {
        let mut e = d;
        let mut parens = 0;
        while let Some(&b) = bytes.get(e) {
            match b {
                b'\\' => e += 1,
                b'(' => parens += 1,
                b')' if parens == 0 => break,
                b')' => parens -= 1,
                b' ' | b'\t' | b'\n' => break,
                _ => {}
            }
            e += 1;
        }
        (d, e.min(text.len()))
    };
    let raw_dest = &text[dest_start..dest_end];
    if raw_dest.is_empty() || is_external(raw_dest) {
        return None;
    }
    let (path_raw, subpath) = match raw_dest.find('#') {
        Some(h) => (&raw_dest[..h], Some(decode(&raw_dest[h + 1..]))),
        None => (raw_dest, None),
    };
    if path_raw.is_empty() {
        return None;
    }
    let display = text[if embed { 2 } else { 1 }..close_text].to_string();
    Some(Link {
        kind: LinkKind::Markdown,
        embed,
        target: decode(path_raw),
        subpath: subpath.filter(|s| !s.is_empty()),
        display: Some(display),
        span: range.clone(),
        target_span: range.start + dest_start..range.start + dest_start + path_raw.len(),
        line: lines.line_of(range.start),
        node: None,
    })
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// A destination that leaves the vault: any `scheme:` (http, mailto,
/// obsidian://, file:) — but not a Windows drive letter, which nobody writes
/// in a vault link anyway.
fn is_external(dest: &str) -> bool {
    match dest.find(':') {
        Some(c) if c > 1 => dest[..c]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '+' || ch == '-' || ch == '.'),
        _ => false,
    }
}

fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '/'
}

/// `#tag` in the body: `#` at a line start or after whitespace, then tag
/// characters, at least one of them not a digit (`#123` is not a tag).
/// `# Heading` never matches because the space is not a tag character.
fn scan_tags(
    src: &str,
    from: usize,
    skip: &Ranges,
    links: &Ranges,
    lines: &LineIndex,
    out: &mut Vec<NoteTag>,
) {
    let mut prev: Option<char> = src[..from].chars().next_back();
    let mut iter = src[from..].char_indices().map(|(i, c)| (i + from, c)).peekable();
    while let Some((i, c)) = iter.next() {
        let at_boundary = prev.is_none_or(|p| p.is_whitespace());
        prev = Some(c);
        if c != '#' || !at_boundary || skip.contains(i) || links.contains(i) {
            continue;
        }
        let rest = &src[i + 1..];
        let len: usize = rest.chars().take_while(|&c| is_tag_char(c)).map(char::len_utf8).sum();
        let name = rest[..len].trim_end_matches('/');
        if name.is_empty() || name.chars().all(|c| c.is_ascii_digit() || c == '/') {
            continue;
        }
        out.push(NoteTag { name: name.to_string(), line: Some(lines.line_of(i)) });
        // Skip past the tag so `#a#b` yields one tag, as Obsidian reads it.
        while iter.peek().is_some_and(|&(j, _)| j <= i + len) {
            prev = iter.next().map(|(_, c)| c);
        }
    }
}

/// Line-shaped things: tasks and `^block` ids.
fn scan_lines(src: &str, from: usize, skip: &Ranges, lines: &LineIndex, note: &mut Note) {
    let mut start = from;
    for line in src[from..].split_inclusive('\n') {
        let line_start = start;
        start += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        if skip.contains(line_start) {
            continue;
        }
        let line_no = lines.line_of(line_start);
        if let Some((status_at, status, rest)) = task_parts(text) {
            if !skip.contains(line_start + status_at) {
                note.tasks.push(Task {
                    status,
                    text: rest.trim().to_string(),
                    line: line_no,
                    status_at: line_start + status_at,
                });
            }
        }
        if let Some(id) = block_id(text) {
            if !skip.contains(line_start + text.len() - 1) {
                note.blocks.push(BlockId { id: id.to_string(), line: line_no });
            }
        }
    }
}

/// `- [ ] text`, `* [x] text`, `1. [/] text`, also inside `> ` quotes and
/// callouts. Returns (byte offset of the status char in the line, status,
/// the text after the box).
pub fn task_parts(line: &str) -> Option<(usize, char, &str)> {
    let b = line.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        if i < b.len() && b[i] == b'>' {
            i += 1;
            continue;
        }
        break;
    }
    match b.get(i)? {
        b'-' | b'*' | b'+' => i += 1,
        b'0'..=b'9' => {
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            if !matches!(b.get(i), Some(b'.') | Some(b')')) {
                return None;
            }
            i += 1;
        }
        _ => return None,
    }
    if !matches!(b.get(i), Some(b' ') | Some(b'\t')) {
        return None;
    }
    while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let status_at = i + 1;
    let status = line[status_at..].chars().next()?;
    let after = status_at + status.len_utf8();
    if b.get(after) != Some(&b']') {
        return None;
    }
    let rest = &line[after + 1..];
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    Some((status_at, status, rest))
}

/// A trailing ` ^block-id` (or a line that is only `^block-id`).
fn block_id(line: &str) -> Option<&str> {
    let t = line.trim_end();
    let caret = t.rfind('^')?;
    let id = &t[caret + 1..];
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    if caret > 0 && !t[..caret].ends_with([' ', '\t']) {
        return None;
    }
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(n: &Note) -> Vec<&str> {
        n.links.iter().map(|l| l.target.as_str()).collect()
    }

    #[test]
    fn wikilink_forms() {
        let src = "See [[Alpha]], [[folder/Beta#Intro|the beta]] and ![[pic.png]].\n\
                   Block: [[Gamma#^abc123]]. Self: [[#Local]].\n";
        let n = parse(src);
        assert_eq!(targets(&n), ["Alpha", "folder/Beta", "pic.png", "Gamma"]);
        let beta = &n.links[1];
        assert_eq!(beta.subpath.as_deref(), Some("Intro"));
        assert_eq!(beta.display.as_deref(), Some("the beta"));
        assert_eq!(&src[beta.target_span.clone()], "folder/Beta");
        assert_eq!(&src[beta.span.clone()], "[[folder/Beta#Intro|the beta]]");
        assert!(n.links[2].embed);
        assert_eq!(&src[n.links[2].span.clone()], "![[pic.png]]");
        assert_eq!(n.links[3].subpath.as_deref(), Some("^abc123"));
        assert_eq!(n.links[3].line, 1);
    }

    #[test]
    fn table_escaped_pipe() {
        let src = "| a | b |\n|---|---|\n| [[Note\\|shown]] | x |\n";
        let n = parse(src);
        assert_eq!(targets(&n), ["Note"]);
        assert_eq!(n.links[0].display.as_deref(), Some("shown"));
        assert_eq!(&src[n.links[0].target_span.clone()], "Note");
    }

    #[test]
    fn code_and_comments_hide_links_and_tags() {
        let src = "```\n[[InFence]] #infence\n```\n\
                   `[[InCode]]` %% [[InComment]] #incomment %%\n\n\
                   \x20   [[Indented]]\n\n[[Real]] #real\n";
        let n = parse(src);
        assert_eq!(targets(&n), ["Real"]);
        let tags: Vec<_> = n.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tags, ["real"]);
    }

    #[test]
    fn markdown_links() {
        let src = "[a](My%20Note.md) [b](<Other Note.md#Part>) ![c](img/x.png)\n\
                   [web](https://example.com) [mail](mailto:a@b.c) [here](#local)\n";
        let n = parse(src);
        assert_eq!(targets(&n), ["My Note.md", "Other Note.md", "img/x.png"]);
        assert_eq!(&src[n.links[0].target_span.clone()], "My%20Note.md");
        assert_eq!(&src[n.links[1].target_span.clone()], "Other Note.md");
        assert_eq!(n.links[1].subpath.as_deref(), Some("Part"));
        assert!(n.links[2].embed);
        assert!(n.links.iter().all(|l| l.kind == LinkKind::Markdown));
    }

    #[test]
    fn tag_rules() {
        let src = "# Heading\n#top and #nested/child, #123 not, a#b not, \
                   #under_score #日本 (see [[Note#Sec]]) #trail/\n";
        let n = parse(src);
        let tags: Vec<_> = n.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tags, ["top", "nested/child", "under_score", "日本", "trail"]);
        assert_eq!(n.headings.len(), 1);
    }

    #[test]
    fn frontmatter_properties_and_tags() {
        let src = "---\ntitle: Hello\ncount: 3\ndone: true\ntags: [one, \"#two\"]\n\
                   aliases:\n  - Hi\n  - Hey\nrelated: \"[[Other]]\"\n---\n# Body #three\n";
        let n = parse(src);
        assert_eq!(n.properties["title"], "Hello");
        assert_eq!(n.properties["count"], 3);
        assert_eq!(n.properties["done"], true);
        assert_eq!(aliases(&n.properties), ["Hi", "Hey"]);
        let tags: Vec<_> = n.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tags, ["one", "two", "three"]);
        assert_eq!(targets(&n), ["Other"]);
        assert_eq!(n.headings[0].text, "Body #three");
        assert_eq!(n.headings[0].line, 10);
        let keys: Vec<_> = n.properties.keys().collect();
        assert_eq!(keys, ["title", "count", "done", "tags", "aliases", "related"]);
    }

    #[test]
    fn tags_as_a_string_and_bad_yaml() {
        let n = parse("---\ntags: a, b c\n---\nx\n");
        let tags: Vec<_> = n.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tags, ["a", "b", "c"]);
        let bad = parse("---\n: : [\n---\nbody [[Link]]\n");
        assert!(bad.properties.is_empty());
        assert!(bad.frontmatter.is_some());
        assert_eq!(targets(&bad), ["Link"]);
    }

    #[test]
    fn no_frontmatter_without_closing_fence() {
        let n = parse("---\nnot: closed\n\n[[A]]\n");
        assert!(n.frontmatter.is_none());
    }

    #[test]
    fn tasks_and_statuses() {
        let src = "- [ ] open one\n* [x] done\n1. [/] half\n> - [-] quoted cancel\n\
                   - [] not a task\n-[ ] not either\n```\n- [ ] in code\n```\n";
        let n = parse(src);
        let got: Vec<_> = n.tasks.iter().map(|t| (t.status, t.text.as_str(), t.line)).collect();
        assert_eq!(
            got,
            [(' ', "open one", 0), ('x', "done", 1), ('/', "half", 2), ('-', "quoted cancel", 3)]
        );
        assert_eq!(&src[n.tasks[1].status_at..n.tasks[1].status_at + 1], "x");
        assert!(n.tasks[0].is_open() && n.tasks[2].is_open());
        assert!(!n.tasks[1].is_open() && !n.tasks[3].is_open());
    }

    #[test]
    fn block_ids_and_headings() {
        let src = "Para one ^p1\n\n^standalone\n\nnot^block\n\n## Two `code`\nSetext\n===\n";
        let n = parse(src);
        let ids: Vec<_> = n.blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["p1", "standalone"]);
        let hs: Vec<_> = n.headings.iter().map(|h| (h.level, h.text.as_str(), h.line)).collect();
        assert_eq!(hs, [(2, "Two code", 6), (1, "Setext", 7)]);
    }

    #[test]
    fn crlf_and_unicode_offsets() {
        let src = "---\r\na: 1\r\n---\r\nÜber [[Zürich]]\r\n- [ ] tâche\r\n";
        let n = parse(src);
        assert_eq!(n.properties["a"], 1);
        assert_eq!(&src[n.links[0].target_span.clone()], "Zürich");
        assert_eq!(n.tasks[0].text, "tâche");
    }
}
