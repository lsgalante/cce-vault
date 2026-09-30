//! Finding notes: a fuzzy match on names for the quick switcher, a
//! full-text scan for the search pane, and unlinked mentions for the
//! backlinks pane.
//!
//! Full text is a scan of the files, not an index. Reading a few thousand
//! notes from the page cache takes tens of milliseconds, spread over the
//! machine's cores; a real index (tantivy) is worth its weight only once a
//! scan is measurably slow on a real vault.

use serde::Serialize;

use crate::index::{par_map, stem, FileKind, Index};
use crate::parse;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NameMatch {
    pub path: String,
    /// What matched: the note's name, one of its aliases, or its path.
    pub matched: String,
    pub score: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LineHit {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileHits {
    pub path: String,
    /// Lines holding a match, capped at [`LINES_PER_FILE`].
    pub lines: Vec<LineHit>,
    /// Matching lines in all, including those past the cap.
    pub total: usize,
}

pub const LINES_PER_FILE: usize = 5;

/// Subsequence match of `query` in `candidate`, case-insensitive. Scores
/// reward consecutive runs, matches at word starts and a match at the very
/// start, and slightly penalise long candidates — the ordering a quick
/// switcher needs so that `mt` finds "Meeting Topics" before "Mortgage".
///
/// The alignment is the best one, not the leftmost: a greedy match would
/// spend the `t` of `mt` on "Mee*t*ing" and never see "*T*opics". A small
/// dynamic programme over (query char, candidate position) finds it; names
/// are short, so O(query × candidate) is nothing.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let c: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = c.iter().map(|ch| ch.to_lowercase().next().unwrap_or(*ch)).collect();
    let n = c.len();
    let gain = |j: usize| -> i64 {
        let word_start = j == 0 || !c[j - 1].is_alphanumeric() || (c[j].is_uppercase() && c[j - 1].is_lowercase());
        1 + if word_start { 8 } else { 0 } + if j == 0 { 6 } else { 0 }
    };
    const NONE: i64 = i64::MIN / 4;
    // prev[j]: best score with the previous query char matched at j.
    let mut prev: Vec<i64> = (0..n).map(|j| if lower[j] == q[0] { gain(j) } else { NONE }).collect();
    for &qc in &q[1..] {
        let mut cur = vec![NONE; n];
        let mut best_before = NONE; // max of prev[..j-1]
        for j in 0..n {
            if j >= 2 {
                best_before = best_before.max(prev[j - 2]);
            }
            if lower[j] != qc {
                continue;
            }
            let run = if j >= 1 && prev[j - 1] > NONE { prev[j - 1] + 5 } else { NONE };
            let gap = best_before;
            let base = run.max(gap);
            if base > NONE {
                cur[j] = base + gain(j);
            }
        }
        prev = cur;
    }
    let best = prev.into_iter().max().filter(|&s| s > NONE)?;
    Some(best * 10 - n as i64)
}

/// Terms of a search: words, and `"quoted phrases"` kept whole, lowercased.
pub fn terms(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = query.trim();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            out.push(r[..end].to_lowercase());
            rest = r.get(end + 1..).unwrap_or("").trim_start();
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push(rest[..end].to_lowercase());
            rest = rest[end..].trim_start();
        }
    }
    out.retain(|t| !t.is_empty());
    out
}

/// Byte offsets where `needle` (already lowercase) occurs in `hay`,
/// case-insensitively, as whole words when `words` is set.
fn find_ci(hay: &str, needle: &str, words: bool) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let first = needle.chars().next().unwrap();
    for (i, ch) in hay.char_indices() {
        if ch.to_lowercase().next() != Some(first) {
            continue;
        }
        let mut hay_chars = hay[i..].chars().flat_map(char::to_lowercase);
        let mut end = i;
        let mut ok = true;
        for n in needle.chars() {
            match hay_chars.next() {
                Some(h) if h == n => {}
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        // Find the byte end: count needle chars through the original.
        let mut taken = 0;
        for (j, ch) in hay[i..].char_indices() {
            if taken >= needle.chars().count() {
                end = i + j;
                break;
            }
            taken += ch.to_lowercase().count();
            end = i + j + ch.len_utf8();
        }
        if words {
            let before = hay[..i].chars().next_back();
            let after = hay[end..].chars().next();
            if before.is_some_and(char::is_alphanumeric) || after.is_some_and(char::is_alphanumeric) {
                continue;
            }
        }
        out.push(i);
    }
    out
}

impl Index {
    /// Notes (and other files) whose name, alias or path fuzzy-matches.
    pub fn find(&self, query: &str, limit: usize) -> Vec<NameMatch> {
        let mut out: Vec<NameMatch> = Vec::new();
        for (path, entry) in self.files() {
            let name = stem(path);
            let mut best: Option<(i64, String)> = None;
            let mut consider = |text: &str, bonus: i64| {
                if let Some(s) = fuzzy_score(query, text) {
                    let s = s + bonus;
                    if best.as_ref().is_none_or(|(b, _)| s > *b) {
                        best = Some((s, text.to_string()));
                    }
                }
            };
            consider(name, 0);
            if let Some(note) = &entry.note {
                for alias in parse::aliases(&note.properties) {
                    consider(&alias, -5);
                }
            }
            // Paths match too, so `proj/meet` narrows by folder, but a name
            // match beats them.
            consider(path, -40);
            // Notes first: an attachment is rarely what a switcher wants.
            let kind_bonus = if entry.kind == FileKind::Attachment { -30 } else { 0 };
            if let Some((score, matched)) = best {
                out.push(NameMatch { path: path.clone(), matched, score: score + kind_bonus });
            }
        }
        out.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.cmp(&b.path)));
        out.truncate(limit);
        out
    }

    /// Notes containing every term of `query`, anywhere in the text or the
    /// path. Notes whose name holds a term come first, then those with the
    /// most matching lines.
    pub fn search(&self, query: &str, limit: usize) -> Vec<FileHits> {
        let terms = terms(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let paths: Vec<&str> = self.notes().map(|(p, _)| p).collect();
        let mut hits = par_map(&paths, |path: &&str| {
            let text = std::fs::read_to_string(self.abs(path)).ok()?;
            let lower_text = text.to_lowercase();
            let lower_path = path.to_lowercase();
            if !terms.iter().all(|t| lower_text.contains(t.as_str()) || lower_path.contains(t.as_str())) {
                return None;
            }
            let mut lines = Vec::new();
            let mut total = 0;
            for (n, line) in text.lines().enumerate() {
                let l = line.to_lowercase();
                if terms.iter().any(|t| l.contains(t.as_str())) {
                    total += 1;
                    if lines.len() < LINES_PER_FILE {
                        lines.push(LineHit { line: n, text: line.trim().to_string() });
                    }
                }
            }
            Some(FileHits { path: path.to_string(), lines, total })
        });
        let name_hit = |h: &FileHits| {
            let n = stem(&h.path).to_lowercase();
            terms.iter().any(|t| n.contains(t.as_str()))
        };
        hits.sort_by(|a, b| {
            name_hit(b).cmp(&name_hit(a)).then(b.total.cmp(&a.total)).then(a.path.cmp(&b.path))
        });
        hits.truncate(limit);
        hits
    }

    /// Places in other notes that name `path` (by its name or an alias) as
    /// a whole word, outside any link, code or frontmatter — the "unlinked
    /// mentions" Obsidian offers to turn into links.
    pub fn unlinked_mentions(&self, path: &str) -> Vec<FileHits> {
        let mut names = vec![stem(path).to_lowercase()];
        if let Some(note) = self.note(path) {
            names.extend(parse::aliases(&note.properties).iter().map(|a| a.to_lowercase()));
        }
        names.retain(|n| n.chars().count() >= 2);
        names.sort();
        names.dedup();
        if names.is_empty() {
            return Vec::new();
        }
        let paths: Vec<&str> = self.notes().map(|(p, _)| p).filter(|p| *p != path).collect();
        let mut out = par_map(&paths, |source: &&str| {
            let text = std::fs::read_to_string(self.abs(source)).ok()?;
            let lower = text.to_lowercase();
            if !names.iter().any(|n| lower.contains(n.as_str())) {
                return None;
            }
            // Parse fresh: the spans must match the text just read.
            let note = parse::parse(&text);
            let body_start = note.frontmatter.as_ref().map(|r| r.end).unwrap_or(0);
            let linked = |at: usize| note.links.iter().any(|l| l.span.contains(&at));
            let lines = parse::LineIndex::new(&text);
            let code = code_ranges(&text);
            let mut hit_lines: Vec<usize> = Vec::new();
            for name in &names {
                for at in find_ci(&text, name, true) {
                    if at < body_start || linked(at) || code.iter().any(|r| r.contains(&at)) {
                        continue;
                    }
                    hit_lines.push(lines.line_of(at));
                }
            }
            hit_lines.sort();
            hit_lines.dedup();
            if hit_lines.is_empty() {
                return None;
            }
            let all: Vec<&str> = text.lines().collect();
            Some(FileHits {
                path: source.to_string(),
                total: hit_lines.len(),
                lines: hit_lines
                    .iter()
                    .take(LINES_PER_FILE)
                    .map(|&n| LineHit { line: n, text: all.get(n).unwrap_or(&"").trim().to_string() })
                    .collect(),
            })
        });
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }
}

/// Code blocks and spans, where a mention is not prose.
fn code_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    use pulldown_cmark::{Event, Options, Parser, Tag};
    Parser::new_ext(text, Options::ENABLE_YAML_STYLE_METADATA_BLOCKS)
        .into_offset_iter()
        .filter_map(|(e, r)| match e {
            Event::Start(Tag::CodeBlock(_)) | Event::Code(_) => Some(r),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(files: &[(&str, &str)]) -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let index = Index::open(dir.path(), false).unwrap();
        (dir, index)
    }

    #[test]
    fn fuzzy_ordering() {
        assert!(fuzzy_score("mtg", "Meeting notes").is_some());
        assert!(fuzzy_score("xyz", "Meeting").is_none());
        let a = fuzzy_score("mt", "Meeting Topics").unwrap();
        let b = fuzzy_score("mt", "mortgage").unwrap();
        assert!(a > b, "{a} {b}");
        assert!(fuzzy_score("dn", "DailyNotes").unwrap() > fuzzy_score("dn", "Adenine").unwrap());
    }

    #[test]
    fn find_names_aliases_paths() {
        let (_d, ix) = vault(&[
            ("Meeting Topics.md", ""),
            ("Mortgage.md", ""),
            ("people/Robert.md", "---\naliases: [Bob]\n---\n"),
            ("img/meeting.png", ""),
        ]);
        let got: Vec<_> = ix.find("mt", 10).into_iter().map(|m| m.path).collect();
        assert_eq!(got[0], "Meeting Topics.md");
        let bob = ix.find("bob", 1);
        assert_eq!((bob[0].path.as_str(), bob[0].matched.as_str()), ("people/Robert.md", "Bob"));
        let meet: Vec<_> = ix.find("meeting", 10).into_iter().map(|m| m.path).collect();
        assert_eq!(meet, ["Meeting Topics.md", "img/meeting.png"]);
        assert_eq!(ix.find("people/rob", 1)[0].path, "people/Robert.md");
    }

    #[test]
    fn full_text() {
        let (_d, ix) = vault(&[
            ("a.md", "The quick brown fox\nsecond line\nfox again"),
            ("b.md", "quick but no animal"),
            ("Fox facts.md", "nothing quick here"),
        ]);
        assert_eq!(terms(r#"quick "brown fox"  x"#), ["quick", "brown fox", "x"]);
        let r = ix.search("quick fox", 10);
        let paths: Vec<_> = r.iter().map(|h| h.path.as_str()).collect();
        // A name hit ranks first; b.md lacks "fox".
        assert_eq!(paths, ["Fox facts.md", "a.md"]);
        assert_eq!(r[1].total, 2);
        assert_eq!(r[1].lines[0], LineHit { line: 0, text: "The quick brown fox".into() });
        assert!(ix.search("\"brown fox\"", 10).len() == 1);
        assert!(ix.search("   ", 10).is_empty());
    }

    #[test]
    fn unlinked() {
        let (_d, ix) = vault(&[
            ("Rust.md", "---\naliases: [rustlang]\n---\n"),
            ("a.md", "---\ntopic: Rust\n---\nI like Rust.\n[[Rust]] is linked\n`Rust` in code\nTrusty is not\n"),
            ("b.md", "all about RUSTLANG today"),
            ("c.md", "nothing"),
        ]);
        let m = ix.unlinked_mentions("Rust.md");
        let got: Vec<_> = m.iter().map(|h| (h.path.as_str(), h.lines.iter().map(|l| l.line).collect::<Vec<_>>())).collect();
        assert_eq!(got, [("a.md", vec![3]), ("b.md", vec![0])]);
    }

    #[test]
    fn case_insensitive_offsets() {
        assert_eq!(find_ci("Straße STRASSE", "straße", true), [0]);
        assert_eq!(find_ci("ÄRGER ärger", "ärger", true), [0, 7]);
        assert_eq!(find_ci("arust rust", "rust", true), [6]);
    }
}
