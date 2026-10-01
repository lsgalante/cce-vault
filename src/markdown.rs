//! A note as a reading view needs it: blocks of styled spans, with Obsidian's
//! additions (wikilinks, embeds, `==highlights==`, `#tags`, callouts, custom
//! task statuses, `%%comments%%` hidden) and the source line of every block
//! and task so a click can act on the file.
//!
//! This is a document model, not a renderer: it knows nothing about fonts or
//! pixels, so the notes editor, the desktop's note cards and the graph's hover
//! previews can each lay it out their own way.
//!
//! Unlike [`crate::parse`], which scans raw bytes so a rename can rewrite
//! links in place, this uses pulldown-cmark's own `ENABLE_WIKILINKS` — a
//! renderer needs the link's display text and nesting, not its byte span.
//! Obsidian treats a single newline as a line break (its default
//! "strict line breaks" is off), and so does this.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};

use crate::parse::{self, LineIndex};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub highlight: bool,
    pub math: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanLink {
    /// A note or file in the vault: `[[target#subpath|…]]` or a relative
    /// markdown link. Resolve with [`crate::Index::resolve_text`].
    Note { target: String, subpath: Option<String> },
    /// `![[target]]` / `![alt](target)` inline in a paragraph.
    Embed { target: String, subpath: Option<String> },
    Url(String),
    /// A `#tag`, without the `#`.
    Tag(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub link: Option<SpanLink>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    /// The status character of a task item (`' '`, `'x'`, `'/'`, …).
    pub task: Option<char>,
    /// 0-based source line of the item's marker; with `task`, what
    /// [`crate::Index::set_task`] takes.
    pub line: usize,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Callout {
    /// Lowercased type: `note`, `warning`, `tip`, …
    pub kind: String,
    pub title: Vec<Span>,
    /// `Some(true)` for `[!x]-` (collapsed), `Some(false)` for `[!x]+`.
    pub folded: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Frontmatter, as (key, value shown as text) pairs.
    Properties(Vec<(String, String)>),
    Heading { level: u8, spans: Vec<Span>, line: usize },
    Paragraph { spans: Vec<Span>, line: usize },
    /// A paragraph that is nothing but one `![[embed]]`.
    Embed { target: String, subpath: Option<String>, line: usize },
    List { start: Option<u64>, items: Vec<ListItem>, line: usize },
    Quote { callout: Option<Callout>, blocks: Vec<Block>, line: usize },
    Code { lang: Option<String>, text: String, line: usize },
    Table { header: Vec<Vec<Span>>, rows: Vec<Vec<Vec<Span>>>, line: usize },
    Rule { line: usize },
}

/// Blank out `range` of `text` without moving any line: every byte but
/// newlines becomes a space. Keeps pulldown's offsets on the file's lines.
fn blank(text: &mut String, range: std::ops::Range<usize>) {
    let replaced: String =
        text[range.clone()].chars().map(|c| if c == '\n' || c == '\r' { c } else { ' ' }).collect();
    text.replace_range(range, &replaced);
}

/// `%%comment%%` ranges outside fenced code, found on the raw text.
fn comments(src: &str) -> Vec<std::ops::Range<usize>> {
    let mut code: Vec<std::ops::Range<usize>> = Vec::new();
    for (e, r) in Parser::new_ext(src, Options::empty()).into_offset_iter() {
        if matches!(e, Event::Start(Tag::CodeBlock(_)) | Event::Code(_)) {
            code.push(r);
        }
    }
    let in_code = |at: usize| code.iter().any(|r| r.contains(&at));
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = src[pos..].find("%%") {
        let start = pos + i;
        if in_code(start) {
            pos = start + 2;
            continue;
        }
        let end = src[start + 2..].find("%%").map(|j| start + 2 + j + 2).unwrap_or(src.len());
        out.push(start..end);
        pos = end;
    }
    out
}

enum Frame {
    Root(Vec<Block>),
    Quote { blocks: Vec<Block>, line: usize },
    List { start: Option<u64>, items: Vec<ListItem>, line: usize },
    Item { task: Option<char>, blocks: Vec<Block>, line: usize },
}

impl Frame {
    fn blocks(&mut self) -> &mut Vec<Block> {
        match self {
            Frame::Root(b) | Frame::Quote { blocks: b, .. } | Frame::Item { blocks: b, .. } => b,
            Frame::List { .. } => unreachable!("a list holds items, not blocks"),
        }
    }
}

struct Builder<'a> {
    src: &'a str,
    lines: LineIndex,
    frames: Vec<Frame>,
    spans: Vec<Span>,
    text: String,
    style: Style,
    links: Vec<SpanLink>,
    /// Where the inline run being collected began, when inside a block.
    inline_line: Option<usize>,
    code: Option<(Option<String>, String, usize)>,
    table: Option<TableBuild>,
}

#[derive(Default)]
struct TableBuild {
    header: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    /// Cells of the row being read.
    row: Vec<Vec<Span>>,
    line: usize,
}

impl<'a> Builder<'a> {
    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("root frame")
    }

    /// Turn buffered text into spans: `#tags` and `==highlights==` are
    /// split out here, since pulldown knows neither.
    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        let link = self.links.last().cloned();
        let plain = link.is_none() && !self.style.code && !self.style.math;
        let mut style = self.style;
        let pieces: Vec<&str> = if plain && text.matches("==").count() >= 2 {
            text.split("==").collect()
        } else {
            vec![text.as_str()]
        };
        let toggles = pieces.len() > 1;
        let last = pieces.len() - 1;
        for (i, piece) in pieces.into_iter().enumerate() {
            if plain {
                push_with_tags(&mut self.spans, piece, style);
            } else {
                push_span(&mut self.spans, piece, style, link.clone());
            }
            // An odd trailing `==` has no partner: put it back as text.
            if toggles && i < last {
                if i == last - 1 && !style.highlight && last % 2 == 1 {
                    push_span(&mut self.spans, "==", style, None);
                } else {
                    style.highlight = !style.highlight;
                }
            }
        }
    }

    fn take_spans(&mut self) -> Vec<Span> {
        self.flush_text();
        let mut spans = std::mem::take(&mut self.spans);
        // Trailing line breaks carry nothing.
        while spans.last().is_some_and(|s| s.text.trim_matches('\n').is_empty() && s.text.contains('\n')) {
            spans.pop();
        }
        spans
    }

    /// Tight list items put their text straight in the item; close it into
    /// a paragraph before anything else lands there.
    fn close_loose_inline(&mut self) {
        if let Some(line) = self.inline_line.take() {
            let spans = self.take_spans();
            if !spans.is_empty() {
                let block = paragraph_or_embed(spans, line);
                self.top().blocks().push(block);
            }
        }
    }

    fn push_block(&mut self, block: Block) {
        self.top().blocks().push(block);
    }

    fn line(&self, at: usize) -> usize {
        self.lines.line_of(at.min(self.src.len().saturating_sub(1)))
    }
}

fn push_span(spans: &mut Vec<Span>, text: &str, style: Style, link: Option<SpanLink>) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.style == style && last.link == link && !matches!(link, Some(SpanLink::Tag(_))) {
            last.text.push_str(text);
            return;
        }
    }
    spans.push(Span { text: text.to_string(), style, link });
}

/// Push plain text, turning `#tags` into tag spans by the index's rules.
fn push_with_tags(spans: &mut Vec<Span>, text: &str, style: Style) {
    let mut start = 0;
    let mut prev: Option<char> = spans.last().and_then(|s| s.text.chars().next_back());
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let boundary = prev.is_none_or(char::is_whitespace);
        prev = Some(c);
        if c != '#' || !boundary {
            continue;
        }
        let rest = &text[i + 1..];
        let len: usize = rest
            .chars()
            .take_while(|&c| c.is_alphanumeric() || c == '_' || c == '-' || c == '/')
            .map(char::len_utf8)
            .sum();
        let name = rest[..len].trim_end_matches('/');
        if name.is_empty() || name.chars().all(|c| c.is_ascii_digit() || c == '/') {
            continue;
        }
        push_span(spans, &text[start..i], style, None);
        push_span(spans, &text[i..i + 1 + name.len()], style, Some(SpanLink::Tag(name.to_string())));
        start = i + 1 + name.len();
        while iter.peek().is_some_and(|&(j, _)| j < start) {
            prev = iter.next().map(|(_, c)| c);
        }
    }
    push_span(spans, &text[start..], style, None);
}

fn paragraph_or_embed(spans: Vec<Span>, line: usize) -> Block {
    if let [Span { link: Some(SpanLink::Embed { target, subpath }), .. }] = spans.as_slice() {
        return Block::Embed { target: target.clone(), subpath: subpath.clone(), line };
    }
    Block::Paragraph { spans, line }
}

fn split_target(dest: &str) -> (String, Option<String>) {
    match dest.split_once('#') {
        Some((t, s)) => (t.trim().to_string(), Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        None => (dest.trim().to_string(), None),
    }
}

fn is_url(dest: &str) -> bool {
    dest.contains("://") || dest.starts_with("mailto:")
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// `[!type]+ Title` at the start of a quote's first paragraph.
fn take_callout(blocks: &mut Vec<Block>) -> Option<Callout> {
    let Some(Block::Paragraph { spans, .. }) = blocks.first_mut() else { return None };
    let first = spans.first()?;
    let rest = first.text.strip_prefix("[!")?;
    let close = rest.find(']')?;
    let kind = rest[..close].trim().to_lowercase();
    if kind.is_empty() || kind.contains(char::is_whitespace) {
        return None;
    }
    let mut after = &rest[close + 1..];
    let folded = match after.chars().next() {
        Some('-') => Some(true),
        Some('+') => Some(false),
        _ => None,
    };
    if folded.is_some() {
        after = &after[1..];
    }
    // The title runs to the first line break; what follows is the body.
    let mut title = Vec::new();
    let mut body = Vec::new();
    let mut in_title = true;
    let first_rest = after.trim_start().to_string();
    let style = first.style;
    let mut all: Vec<Span> = Vec::new();
    push_span(&mut all, &first_rest, style, None);
    all.extend(spans.drain(1..));
    for span in all {
        if in_title {
            if let Some((t, b)) = span.text.split_once('\n') {
                push_span(&mut title, t, span.style, span.link.clone());
                in_title = false;
                push_span(&mut body, b, span.style, span.link.clone());
                continue;
            }
            title.push(span);
        } else {
            body.push(span);
        }
    }
    if body.iter().all(|s| s.text.trim().is_empty()) {
        blocks.remove(0);
    } else if let Some(Block::Paragraph { spans, .. }) = blocks.first_mut() {
        *spans = body;
    }
    Some(Callout { kind, title, folded })
}

/// A task status written as `[/] text` at the start of an item, which
/// pulldown only recognises for `[ ]` and `[x]`.
fn take_custom_status(blocks: &mut [Block]) -> Option<char> {
    let Some(Block::Paragraph { spans, .. }) = blocks.first_mut() else { return None };
    let first = spans.first_mut()?;
    let t = first.text.as_str();
    let mut chars = t.chars();
    if chars.next() != Some('[') {
        return None;
    }
    let status = chars.next()?;
    if chars.next() != Some(']') || !matches!(chars.next(), Some(' ') | None) {
        return None;
    }
    let cut = 3 + status.len_utf8() - 1 + if t.len() > 2 + status.len_utf8() { 1 } else { 0 };
    first.text = t[cut.min(t.len())..].to_string();
    Some(status)
}

/// Parse a note into blocks.
pub fn blocks(src: &str) -> Vec<Block> {
    let mut text = src.to_string();
    let mut out_props = None;
    if let Some((whole, inner)) = parse::frontmatter_range(src) {
        let props = parse::parse_properties(&src[inner]);
        out_props = Some(
            props
                .iter()
                .map(|(k, v)| {
                    let shown = match v {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(a) => a
                            .iter()
                            .map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))
                            .collect::<Vec<_>>()
                            .join(", "),
                        serde_json::Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    (k.clone(), shown)
                })
                .collect::<Vec<_>>(),
        );
        blank(&mut text, whole);
    }
    for r in comments(&text) {
        blank(&mut text, r);
    }

    let mut b = Builder {
        src: &text,
        lines: LineIndex::new(&text),
        frames: vec![Frame::Root(Vec::new())],
        spans: Vec::new(),
        text: String::new(),
        style: Style::default(),
        links: Vec::new(),
        inline_line: None,
        code: None,
        table: None,
    };
    if let Some(props) = out_props.filter(|p| !p.is_empty()) {
        b.top().blocks().push(Block::Properties(props));
    }

    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_WIKILINKS;
    let src_copy = text.clone();
    for (event, range) in Parser::new_ext(&src_copy, opts).into_offset_iter() {
        if let Some((_, buf, _)) = b.code.as_mut() {
            match event {
                Event::Text(t) => {
                    buf.push_str(&t);
                    continue;
                }
                Event::End(TagEnd::CodeBlock) => {
                    let (lang, mut body, line) = b.code.take().unwrap();
                    if body.ends_with('\n') {
                        body.pop();
                    }
                    b.push_block(Block::Code { lang, text: body, line });
                    continue;
                }
                _ => continue,
            }
        }
        match event {
            Event::Start(Tag::Paragraph) | Event::Start(Tag::Heading { .. }) => {
                b.close_loose_inline();
                b.inline_line = None;
                b.spans.clear();
            }
            Event::End(TagEnd::Paragraph) => {
                let spans = b.take_spans();
                let line = b.line(range.start);
                if !spans.is_empty() {
                    b.push_block(paragraph_or_embed(spans, line));
                }
            }
            Event::End(TagEnd::Heading(level)) => {
                let spans = b.take_spans();
                let line = b.line(range.start);
                let level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                b.push_block(Block::Heading { level, spans, line });
            }
            Event::Start(Tag::BlockQuote(_)) => {
                b.close_loose_inline();
                let line = b.line(range.start);
                b.frames.push(Frame::Quote { blocks: Vec::new(), line });
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                b.close_loose_inline();
                if let Some(Frame::Quote { mut blocks, line }) = b.frames.pop() {
                    let callout = take_callout(&mut blocks);
                    b.push_block(Block::Quote { callout, blocks, line });
                }
            }
            Event::Start(Tag::List(start)) => {
                b.close_loose_inline();
                let line = b.line(range.start);
                b.frames.push(Frame::List { start, items: Vec::new(), line });
            }
            Event::End(TagEnd::List(_)) => {
                if let Some(Frame::List { start, items, line }) = b.frames.pop() {
                    b.push_block(Block::List { start, items, line });
                }
            }
            Event::Start(Tag::Item) => {
                let line = b.line(range.start);
                b.frames.push(Frame::Item { task: None, blocks: Vec::new(), line });
                b.inline_line = Some(line);
            }
            Event::End(TagEnd::Item) => {
                b.close_loose_inline();
                if let Some(Frame::Item { mut task, mut blocks, line }) = b.frames.pop() {
                    if task.is_none() {
                        task = take_custom_status(&mut blocks);
                    }
                    if let Some(Frame::List { items, .. }) = b.frames.last_mut() {
                        items.push(ListItem { task, line, blocks });
                    }
                }
            }
            Event::TaskListMarker(checked) => {
                if let Some(Frame::Item { task, .. }) = b.frames.last_mut() {
                    *task = Some(if checked { 'x' } else { ' ' });
                } else if let Some(Frame::Item { task, .. }) =
                    b.frames.iter_mut().rev().find(|f| matches!(f, Frame::Item { .. }))
                {
                    *task = Some(if checked { 'x' } else { ' ' });
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                b.close_loose_inline();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) if !l.trim().is_empty() => Some(l.trim().to_string()),
                    _ => None,
                };
                let line = b.line(range.start);
                b.code = Some((lang, String::new(), line));
            }
            Event::DisplayMath(m) => {
                b.flush_text();
                let line = b.line(range.start);
                b.close_loose_inline();
                b.push_block(Block::Code { lang: Some("math".into()), text: m.to_string(), line });
            }
            Event::Rule => {
                b.close_loose_inline();
                let line = b.line(range.start);
                b.push_block(Block::Rule { line });
            }
            Event::Start(Tag::Table(_)) => {
                b.close_loose_inline();
                let line = b.line(range.start);
                b.table = Some(TableBuild { line, ..Default::default() });
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(t) = b.table.as_mut() {
                    t.header = std::mem::take(&mut t.row);
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(t) = b.table.as_mut() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            Event::End(TagEnd::TableCell) => {
                let spans = b.take_spans();
                if let Some(t) = b.table.as_mut() {
                    t.row.push(spans);
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(t) = b.table.take() {
                    b.push_block(Block::Table { header: t.header, rows: t.rows, line: t.line });
                }
            }
            Event::Start(Tag::Emphasis) => {
                b.flush_text();
                b.style.italic = true;
            }
            Event::End(TagEnd::Emphasis) => {
                b.flush_text();
                b.style.italic = false;
            }
            Event::Start(Tag::Strong) => {
                b.flush_text();
                b.style.bold = true;
            }
            Event::End(TagEnd::Strong) => {
                b.flush_text();
                b.style.bold = false;
            }
            Event::Start(Tag::Strikethrough) => {
                b.flush_text();
                b.style.strike = true;
            }
            Event::End(TagEnd::Strikethrough) => {
                b.flush_text();
                b.style.strike = false;
            }
            Event::Start(Tag::Link { link_type, dest_url, .. }) => {
                b.flush_text();
                let link = if matches!(link_type, LinkType::WikiLink { .. }) {
                    let (target, subpath) = split_target(&dest_url);
                    SpanLink::Note { target, subpath }
                } else if is_url(&dest_url) || matches!(link_type, LinkType::Autolink | LinkType::Email) {
                    let url = if link_type == LinkType::Email { format!("mailto:{dest_url}") } else { dest_url.to_string() };
                    SpanLink::Url(url)
                } else {
                    let (target, subpath) = split_target(&decode(&dest_url));
                    SpanLink::Note { target, subpath }
                };
                b.links.push(link);
            }
            Event::End(TagEnd::Link) => {
                b.flush_text();
                b.links.pop();
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                b.flush_text();
                let link = if is_url(&dest_url) {
                    SpanLink::Url(dest_url.to_string())
                } else {
                    let (target, subpath) = split_target(&decode(&dest_url));
                    SpanLink::Embed { target, subpath }
                };
                b.links.push(link);
            }
            Event::End(TagEnd::Image) => {
                // An embed with no alt text shows its target.
                if b.text.is_empty() {
                    if let Some(SpanLink::Embed { target, .. } | SpanLink::Url(target)) = b.links.last() {
                        b.text = target.clone();
                    }
                }
                b.flush_text();
                b.links.pop();
            }
            Event::Text(t) => {
                if b.inline_line.is_none() && matches!(b.frames.last(), Some(Frame::Item { .. })) {
                    b.inline_line = Some(b.line(range.start));
                }
                b.text.push_str(&t);
            }
            Event::Code(t) => {
                b.flush_text();
                let link = b.links.last().cloned();
                push_span(&mut b.spans, &t, Style { code: true, ..b.style }, link);
            }
            Event::InlineMath(t) => {
                b.flush_text();
                push_span(&mut b.spans, &t, Style { math: true, ..b.style }, None);
            }
            Event::SoftBreak | Event::HardBreak => {
                b.text.push('\n');
            }
            Event::Html(t) | Event::InlineHtml(t) => b.text.push_str(&t),
            Event::FootnoteReference(label) => {
                b.flush_text();
                push_span(&mut b.spans, &format!("[{label}]"), Style { code: false, ..b.style }, None);
            }
            _ => {}
        }
    }
    b.close_loose_inline();
    match b.frames.into_iter().next() {
        Some(Frame::Root(blocks)) => blocks,
        _ => Vec::new(),
    }
}

/// The plain text of some spans, for tests, titles and search snippets.
pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(b: &Block) -> &[Span] {
        match b {
            Block::Paragraph { spans, .. } => spans,
            other => panic!("not a paragraph: {other:?}"),
        }
    }

    #[test]
    fn inline_styles_links_tags() {
        let doc = blocks("Some **bold** and *it* ~~gone~~ `code` ==mark== #tag/x and [[Note#Sec|shown]] [web](https://a.b) [md](Other%20Note.md)\n");
        let spans = para(&doc[0]);
        let find = |t: &str| spans.iter().find(|s| s.text == t).unwrap_or_else(|| panic!("{t} in {spans:?}"));
        assert!(find("bold").style.bold);
        assert!(find("it").style.italic);
        assert!(find("gone").style.strike);
        assert!(find("code").style.code);
        assert!(find("mark").style.highlight);
        assert_eq!(find("#tag/x").link, Some(SpanLink::Tag("tag/x".into())));
        assert_eq!(find("shown").link, Some(SpanLink::Note { target: "Note".into(), subpath: Some("Sec".into()) }));
        assert_eq!(find("web").link, Some(SpanLink::Url("https://a.b".into())));
        assert_eq!(find("md").link, Some(SpanLink::Note { target: "Other Note.md".into(), subpath: None }));
    }

    #[test]
    fn single_newline_is_a_break_and_odd_highlight_stays() {
        let doc = blocks("one\ntwo == three\n");
        assert_eq!(plain(para(&doc[0])), "one\ntwo == three");
        assert!(para(&doc[0]).iter().all(|s| !s.style.highlight));
    }

    #[test]
    fn frontmatter_comments_and_lines() {
        let doc = blocks("---\ntitle: T\ntags: [a, b]\n---\n# Head\n%% hidden\nstill hidden %%\nBody [[Link]]\n");
        assert_eq!(doc[0], Block::Properties(vec![("title".into(), "T".into()), ("tags".into(), "a, b".into())]));
        match &doc[1] {
            Block::Heading { level: 1, spans, line: 4 } => assert_eq!(plain(spans), "Head"),
            other => panic!("{other:?}"),
        }
        match &doc[2] {
            Block::Paragraph { spans, line } => {
                assert_eq!(plain(spans).trim(), "Body Link");
                assert_eq!(*line, 7);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn lists_tasks_and_custom_statuses() {
        let doc = blocks("- [ ] open\n- [x] done\n- [/] half\n- plain\n  - nested\n1. first\n");
        let Block::List { start: None, items, .. } = &doc[0] else { panic!("{doc:?}") };
        let got: Vec<_> = items.iter().map(|i| (i.task, i.line, plain(para(&i.blocks[0])))).collect();
        assert_eq!(
            got,
            [
                (Some(' '), 0, "open".to_string()),
                (Some('x'), 1, "done".to_string()),
                (Some('/'), 2, "half".to_string()),
                (None, 3, "plain".to_string()),
            ]
        );
        let Block::List { items: nested, .. } = &items[3].blocks[1] else { panic!("{:?}", items[3]) };
        assert_eq!(plain(para(&nested[0].blocks[0])), "nested");
        assert!(matches!(doc[1], Block::List { start: Some(1), .. }));
    }

    #[test]
    fn callouts_and_quotes() {
        let doc = blocks("> [!Warning]- Careful **now**\n> body text\n\n> plain quote\n");
        let Block::Quote { callout: Some(c), blocks, .. } = &doc[0] else { panic!("{doc:?}") };
        assert_eq!(c.kind, "warning");
        assert_eq!(c.folded, Some(true));
        assert_eq!(plain(&c.title), "Careful now");
        assert_eq!(plain(para(&blocks[0])), "body text");
        assert!(matches!(&doc[1], Block::Quote { callout: None, .. }));
    }

    #[test]
    fn code_tables_embeds_rules() {
        let doc = blocks("```rust\nfn x() {}\n```\n\n| a | b |\n|---|---|\n| [[L]] | 2 |\n\n![[Pic.png]]\n\n---\n");
        assert_eq!(doc[0], Block::Code { lang: Some("rust".into()), text: "fn x() {}".into(), line: 0 });
        let Block::Table { header, rows, .. } = &doc[1] else { panic!("{doc:?}") };
        assert_eq!(header.iter().map(|c| plain(c)).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(rows[0][0][0].link, Some(SpanLink::Note { target: "L".into(), subpath: None }));
        assert_eq!(doc[2], Block::Embed { target: "Pic.png".into(), subpath: None, line: 8 });
        assert!(matches!(doc[3], Block::Rule { line: 10 }));
    }
}
