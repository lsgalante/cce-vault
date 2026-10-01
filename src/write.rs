//! Writing to the vault: atomic file writes, creating notes, ticking tasks,
//! and renaming a file while rewriting every link that points at it.
//!
//! Every edit re-reads the file it changes and re-parses it at that moment
//! rather than trusting the index's copy. The index may be a watcher batch
//! behind — Obsidian or a sync client may have written the file a second
//! ago — and a byte span taken from a stale parse would cut the wrong text.

use std::io;
use std::ops::Range;
use std::path::Path;

use crate::canvas;
use crate::index::{parent, stem, FileKind, Index};
use crate::parse::{self, Link, LinkKind};

#[derive(Debug)]
pub enum WriteError {
    NotFound(String),
    Exists(String),
    InvalidPath(String),
    NoTask { path: String, line: usize },
    Canvas(String),
    Io(io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::NotFound(p) => write!(f, "no such file in the vault: {p}"),
            WriteError::Exists(p) => write!(f, "already exists: {p}"),
            WriteError::InvalidPath(p) => write!(f, "not a usable vault path: {p}"),
            WriteError::NoTask { path, line } => write!(f, "{path}:{}: no task on that line", line + 1),
            WriteError::Canvas(e) => write!(f, "{e}"),
            WriteError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WriteError {}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        WriteError::Io(e)
    }
}

/// Write via a hidden temp file in the same folder and a rename, so a
/// reader (Obsidian, a sync client, another cce app) never sees half a
/// file. The temp name starts with `.` so the vault walk and the watcher
/// both ignore it.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.cce-tmp", std::process::id()));
    let result = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// One link rewritten by a rename.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LinkEdit {
    /// The file holding the link, under its name after the rename.
    pub path: String,
    /// 0-based line of the link (0 for a canvas file node).
    pub line: usize,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RenamePlan {
    pub from: String,
    pub to: String,
    pub edits: Vec<LinkEdit>,
}

/// A vault path a caller may create: relative, no `..`, no hidden part.
fn check_new_path(to: &str) -> Result<String, WriteError> {
    let clean = to.trim().trim_start_matches('/');
    let bad = clean.is_empty()
        || clean.ends_with('/')
        || clean.split('/').any(|s| s.is_empty() || s == "." || s == ".." || s.starts_with('.'));
    if bad {
        return Err(WriteError::InvalidPath(to.to_string()));
    }
    Ok(clean.to_string())
}

fn read_text(path: &Path) -> io::Result<String> {
    let bytes = std::fs::read(path)?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Apply byte-range replacements, last first so earlier spans stay valid.
fn splice(text: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    for (range, with) in edits {
        out.replace_range(range, &with);
    }
    out
}

/// `../../x/y.md` from folder `from_dir` to vault path `to`.
fn relative_path(from_dir: &str, to: &str) -> String {
    let a: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<&str> = vec![".."; a.len() - common];
    parts.extend(&b[common..]);
    parts.join("/")
}

/// Percent-encode what would break a bare markdown destination.
fn encode_dest(path: &str) -> String {
    path.replace('%', "%25").replace(' ', "%20").replace('(', "%28").replace(')', "%29")
}

impl Index {
    /// Create a new note (or any file) with this content.
    pub fn create(&mut self, path: &str, content: &str) -> Result<String, WriteError> {
        let rel = check_new_path(path)?;
        if self.lookup_exact(&rel).is_some() {
            return Err(WriteError::Exists(rel));
        }
        atomic_write(&self.abs(&rel), content.as_bytes())?;
        self.refresh(std::slice::from_ref(&rel));
        Ok(rel)
    }

    /// A file's current text, read from disk (not from the index).
    pub fn read_text(&self, path: &str) -> io::Result<String> {
        read_text(&self.abs(path))
    }

    /// Replace a note's whole text — an editor's save. Creates the file
    /// (and its folders) when it does not exist yet.
    pub fn write_text(&mut self, path: &str, text: &str) -> Result<(), WriteError> {
        let rel = check_new_path(path)?;
        atomic_write(&self.abs(&rel), text.as_bytes())?;
        self.refresh(&[rel]);
        Ok(())
    }

    /// Append a paragraph to a note, creating the note if it is missing.
    pub fn append(&mut self, path: &str, text: &str) -> Result<(), WriteError> {
        let rel = check_new_path(path)?;
        let abs = self.abs(&rel);
        let mut body = match read_text(&abs) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(text);
        if !text.ends_with('\n') {
            body.push('\n');
        }
        atomic_write(&abs, body.as_bytes())?;
        self.refresh(&[rel]);
        Ok(())
    }

    /// Set the status character of the task on `line` (0-based) of a note:
    /// `'x'` to tick it, `' '` to untick.
    pub fn set_task(&mut self, path: &str, line: usize, status: char) -> Result<(), WriteError> {
        if FileKind::of(path) != FileKind::Note {
            return Err(WriteError::NoTask { path: path.to_string(), line });
        }
        let abs = self.abs(path);
        let text = read_text(&abs).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => WriteError::NotFound(path.to_string()),
            _ => e.into(),
        })?;
        let note = parse::parse(&text);
        let task = note
            .tasks
            .iter()
            .find(|t| t.line == line)
            .ok_or_else(|| WriteError::NoTask { path: path.to_string(), line })?;
        let at = task.status_at;
        let old_len = task.status.len_utf8();
        let new = splice(&text, vec![(at..at + old_len, status.to_string())]);
        atomic_write(&abs, new.as_bytes())?;
        self.refresh(&[path.to_string()]);
        Ok(())
    }

    fn lookup_exact(&self, rel: &str) -> Option<&str> {
        let lower = rel.to_lowercase();
        self.files().keys().find(|k| k.to_lowercase() == lower).map(String::as_str)
    }

    /// What [`rename`](Index::rename) would change, without changing it.
    pub fn plan_rename(&self, from: &str, to: &str) -> Result<RenamePlan, WriteError> {
        Ok(self.prepare_rename(from, to)?.plan)
    }

    /// Move `from` to `to` and rewrite every link that pointed at it, in
    /// notes and canvases alike. Links keep their subpath, display text,
    /// embed `!` and `.md` suffix; the path part is written in the shortest
    /// form that still reaches the file, or in full when the link was
    /// written in full. A markdown link keeps being relative or rooted, as
    /// it was.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<RenamePlan, WriteError> {
        let prepared = self.prepare_rename(from, to)?;
        let before: Vec<Option<String>> =
            self.outgoing(from).into_iter().map(|(_, t)| t.map(String::from)).collect();

        let to_abs = self.abs(&prepared.plan.to);
        if let Some(dir) = to_abs.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::rename(self.abs(from), &to_abs)?;
        let mut touched = vec![from.to_string(), prepared.plan.to.clone()];
        for (path, content) in &prepared.writes {
            atomic_write(&self.abs(path), content.as_bytes())?;
            touched.push(path.clone());
        }
        self.refresh(&touched);

        // A move to another folder can change what the moved note's own
        // short links reach (the same-folder preference). Pin any that now
        // land elsewhere to the file they reached before.
        let mut plan = prepared.plan;
        let to = plan.to.clone();
        let fixed = self.pin_moved_links(from, &to, &before)?;
        plan.edits.extend(fixed);
        Ok(plan)
    }

    fn pin_moved_links(
        &mut self,
        from: &str,
        to: &str,
        before: &[Option<String>],
    ) -> Result<Vec<LinkEdit>, WriteError> {
        if parent(from) == parent(to) || FileKind::of(to) != FileKind::Note {
            return Ok(Vec::new());
        }
        let abs = self.abs(to);
        let text = read_text(&abs)?;
        let note = parse::parse(&text);
        let mut edits = Vec::new();
        let mut out = Vec::new();
        for (link, was) in note.links.iter().zip(before) {
            let Some(was) = was else { continue };
            // A self-link was already rewritten to the new name.
            let was = if was == from { to } else { was.as_str() };
            if link.kind != LinkKind::Wiki || self.resolve(Some(to), link).as_deref() == Some(was) {
                continue;
            }
            let keep_md = link.target.to_lowercase().ends_with(".md");
            let new = full_form(was, keep_md);
            out.push(LinkEdit {
                path: to.to_string(),
                line: link.line,
                old: text[link.span.clone()].to_string(),
                new: format!(
                    "{}{}{}",
                    &text[link.span.start..link.target_span.start],
                    new,
                    &text[link.target_span.end..link.span.end]
                ),
            });
            edits.push((link.target_span.clone(), new));
        }
        if !edits.is_empty() {
            atomic_write(&abs, splice(&text, edits).as_bytes())?;
            self.refresh(&[to.to_string()]);
        }
        Ok(out)
    }

    fn prepare_rename(&self, from: &str, to: &str) -> Result<PreparedRename, WriteError> {
        if self.entry(from).is_none() {
            return Err(WriteError::NotFound(from.to_string()));
        }
        let to = check_new_path(to)?;
        if let Some(existing) = self.lookup_exact(&to) {
            // A case-only rename of the same file is fine.
            if existing != from {
                return Err(WriteError::Exists(existing.to_string()));
            }
        }
        let mut plan = RenamePlan { from: from.to_string(), to: to.clone(), edits: Vec::new() };
        let mut writes = Vec::new();

        let mut sources: Vec<&str> = self.backlinks(from).iter().map(|b| b.source).collect();
        // The moved note's own relative markdown links need re-rooting even
        // when nothing links to it.
        if FileKind::of(from) == FileKind::Note {
            sources.push(from);
        }
        sources.sort();
        sources.dedup();

        for source in sources {
            let after = if source == from { to.as_str() } else { source };
            let abs = self.abs(source);
            let text = read_text(&abs)?;
            let new_text = match FileKind::of(source) {
                FileKind::Canvas => self.rename_in_canvas(&text, source, after, from, &to, &mut plan)?,
                _ => self.rename_in_text(&text, source, after, from, &to, None, &mut plan),
            };
            if let Some(new_text) = new_text {
                writes.push((after.to_string(), new_text));
            }
        }
        Ok(PreparedRename { plan, writes })
    }

    /// Rewrite the links in one note's text (or one canvas text node's).
    /// `source` is where the text lives now, `after` where it will live.
    #[allow(clippy::too_many_arguments)]
    fn rename_in_text(
        &self,
        text: &str,
        source: &str,
        after: &str,
        from: &str,
        to: &str,
        node: Option<&str>,
        plan: &mut RenamePlan,
    ) -> Option<String> {
        let note = parse::parse(text);
        let moved_dir = parent(source) != parent(after);
        let mut edits = Vec::new();
        for link in &note.links {
            let Some(target) = self.resolve(Some(source), link) else { continue };
            let new = if target == from {
                self.new_link_path(link, text, after, from, to)
            } else if moved_dir && link.kind == LinkKind::Markdown && is_relative_md(link, source, &target) {
                // The note itself moved: re-root its relative links.
                Some(markdown_dest(link, text, parent(after), &target))
            } else {
                None
            };
            let Some(new) = new else { continue };
            if new == text[link.target_span.clone()] {
                continue;
            }
            plan.edits.push(LinkEdit {
                path: after.to_string(),
                line: if node.is_some() { 0 } else { link.line },
                old: text[link.span.clone()].to_string(),
                new: format!(
                    "{}{}{}",
                    &text[link.span.start..link.target_span.start],
                    new,
                    &text[link.target_span.end..link.span.end]
                ),
            });
            edits.push((link.target_span.clone(), new));
        }
        (!edits.is_empty()).then(|| splice(text, edits))
    }

    fn rename_in_canvas(
        &self,
        text: &str,
        source: &str,
        after: &str,
        from: &str,
        to: &str,
        plan: &mut RenamePlan,
    ) -> Result<Option<String>, WriteError> {
        let mut board = canvas::from_str(text).map_err(|e| WriteError::Canvas(format!("{source}: {e}")))?;
        let mut changed = false;
        for node in board.nodes_mut() {
            let id = node.str("id").unwrap_or_default();
            match node.str("type").as_deref() {
                Some("file") => {
                    let Some(file) = node.str("file") else { continue };
                    if self.resolve_path_exact(&file) == Some(from) {
                        plan.edits.push(LinkEdit { path: after.to_string(), line: 0, old: file, new: to.to_string() });
                        node.set_str("file", to);
                        changed = true;
                    }
                }
                Some("text") => {
                    let body = node.str("text").unwrap_or_default();
                    if let Some(new) = self.rename_in_text(&body, source, after, from, to, Some(&id), plan) {
                        node.set_str("text", &new);
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        Ok(changed.then(|| canvas::to_string(&board)))
    }

    /// A canvas `file` field is always a full vault path.
    fn resolve_path_exact(&self, path: &str) -> Option<&str> {
        self.lookup_exact(path.trim_start_matches('/'))
    }

    /// The new path text for a link that pointed at the renamed file.
    fn new_link_path(&self, link: &Link, text: &str, after: &str, from: &str, to: &str) -> Option<String> {
        let written = &text[link.target_span.clone()];
        match link.kind {
            LinkKind::Wiki => {
                let keep_md = written.to_lowercase().ends_with(".md");
                if written.contains('/') || !self.short_name_reaches(after, from, to) {
                    Some(full_form(to, keep_md))
                } else {
                    let name = stem(to);
                    Some(if keep_md { format!("{name}.md") } else { name.to_string() })
                }
            }
            LinkKind::Markdown => {
                // Rooted if it was written rooted, relative otherwise.
                if written.starts_with('/') {
                    Some(format!("/{}", encode_like(link, text, to)))
                } else {
                    Some(markdown_dest(link, text, parent(after), to))
                }
            }
            LinkKind::CanvasFile => Some(to.to_string()),
        }
    }

    /// Whether `[[name]]` written in `source` would reach `to` once the
    /// rename of `from` is done, with no other file of that name winning.
    fn short_name_reaches(&self, source: &str, from: &str, to: &str) -> bool {
        let key = stem(to).to_lowercase();
        let dir = parent(source);
        let mut rivals: Vec<&str> = self
            .files()
            .keys()
            .map(String::as_str)
            .filter(|p| *p != from && stem(p).to_lowercase() == key && FileKind::of(p) == FileKind::of(to))
            .collect();
        rivals.push(to);
        rivals.sort_by(|a, b| {
            (parent(b) == dir).cmp(&(parent(a) == dir)).then(a.len().cmp(&b.len())).then(a.cmp(b))
        });
        rivals.first() == Some(&to)
    }
}

struct PreparedRename {
    plan: RenamePlan,
    /// (path after the rename, new content)
    writes: Vec<(String, String)>,
}

/// `folder/Note` for a note (with `.md` only if the link had it), the full
/// file name for anything else.
fn full_form(path: &str, keep_md: bool) -> String {
    if FileKind::of(path) == FileKind::Note && !keep_md {
        path[..path.len() - 3].to_string()
    } else {
        path.to_string()
    }
}

/// Was this markdown link written relative to its note (rather than from
/// the vault root)?
fn is_relative_md(link: &Link, source: &str, target: &str) -> bool {
    let decoded = link.target.trim_start_matches("./");
    !link.target.starts_with('/')
        && (link.target.starts_with("../") || format!("{}/{decoded}", parent(source)).trim_start_matches('/') == target)
}

/// A markdown destination for `target` relative to `dir`, encoded the way
/// the original link was.
fn markdown_dest(link: &Link, text: &str, dir: &str, target: &str) -> String {
    encode_like(link, text, &relative_path(dir, target))
}

/// Angle-bracketed destinations are written raw; bare ones are encoded.
fn encode_like(link: &Link, text: &str, path: &str) -> String {
    let angle = link.target_span.start > 0 && text.as_bytes()[link.target_span.start - 1] == b'<';
    if angle {
        path.to_string()
    } else {
        encode_dest(path)
    }
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

    fn read(dir: &tempfile::TempDir, path: &str) -> String {
        std::fs::read_to_string(dir.path().join(path)).unwrap()
    }

    #[test]
    fn rename_rewrites_every_form() {
        let (dir, mut ix) = vault(&[
            ("Old.md", "self: [[Old#Top]]\n"),
            (
                "notes/A.md",
                "[[Old]] [[Old#Sec|shown]] ![[Old]] [[Old.md]] [[old]] `[[Old]]` [[Other]]\n\
                 [md](../Old.md) [md2](<../Old.md#Part>) [root](/Old.md)\n",
            ),
            ("B.canvas", "{\n\t\"nodes\":[\n\t\t{\"id\":\"f\",\"type\":\"file\",\"file\":\"Old.md\",\"x\":0,\"y\":0,\"width\":1,\"height\":1},\n\t\t{\"id\":\"t\",\"type\":\"text\",\"text\":\"see [[Old]]\",\"x\":0,\"y\":0,\"width\":1,\"height\":1}\n\t],\n\t\"edges\":[]\n}"),
            ("Other.md", ""),
        ]);
        let plan = ix.rename("Old.md", "moved/New Name.md").unwrap();
        assert_eq!(plan.to, "moved/New Name.md");
        assert_eq!(
            read(&dir, "notes/A.md"),
            "[[New Name]] [[New Name#Sec|shown]] ![[New Name]] [[New Name.md]] [[New Name]] `[[Old]]` [[Other]]\n\
             [md](../moved/New%20Name.md) [md2](<../moved/New Name.md#Part>) [root](/moved/New%20Name.md)\n"
        );
        assert_eq!(read(&dir, "moved/New Name.md"), "self: [[New Name#Top]]\n");
        let canvas = read(&dir, "B.canvas");
        assert!(canvas.contains("\"file\":\"moved/New Name.md\""), "{canvas}");
        assert!(canvas.contains("\"text\":\"see [[New Name]]\""), "{canvas}");
        assert!(!dir.path().join("Old.md").exists());
        assert_eq!(ix.backlinks("moved/New Name.md").len(), 11);
        assert!(ix.unresolved().is_empty(), "{:?}", ix.unresolved().keys().collect::<Vec<_>>());
    }

    #[test]
    fn ambiguous_new_name_is_written_in_full() {
        let (dir, mut ix) = vault(&[("x/Old.md", ""), ("Taken.md", ""), ("z/L.md", "[[Old]]")]);
        ix.rename("x/Old.md", "x/Taken.md").unwrap();
        // `[[Taken]]` from z/ would reach the shorter Taken.md at the root,
        // so the link names the folder.
        assert_eq!(read(&dir, "z/L.md"), "[[x/Taken]]");
        assert_eq!(ix.resolve_text(Some("z/L.md"), "x/Taken").as_deref(), Some("x/Taken.md"));
    }

    #[test]
    fn moved_note_keeps_its_own_links() {
        let (dir, mut ix) = vault(&[
            ("a/Mover.md", "[[Dup]] [rel](Sib.md) [[Sib]]"),
            ("a/Dup.md", ""),
            ("b/Dup.md", ""),
            ("a/Sib.md", ""),
        ]);
        let plan = ix.rename("a/Mover.md", "b/Mover.md").unwrap();
        // [[Dup]] reached a/Dup.md before; from b/ it would reach b/Dup.md.
        assert_eq!(read(&dir, "b/Mover.md"), "[[a/Dup]] [rel](../a/Sib.md) [[Sib]]");
        assert_eq!(plan.edits.len(), 2);
        let out: Vec<_> = ix.outgoing("b/Mover.md").into_iter().map(|(_, t)| t).collect();
        assert_eq!(out, [Some("a/Dup.md"), Some("a/Sib.md"), Some("a/Sib.md")]);
    }

    #[test]
    fn rename_guards() {
        let (_d, mut ix) = vault(&[("A.md", ""), ("B.md", "")]);
        assert!(matches!(ix.rename("Nope.md", "C.md"), Err(WriteError::NotFound(_))));
        assert!(matches!(ix.rename("A.md", "b.md"), Err(WriteError::Exists(_))));
        assert!(matches!(ix.rename("A.md", "../C.md"), Err(WriteError::InvalidPath(_))));
        assert!(matches!(ix.rename("A.md", ".hidden/C.md"), Err(WriteError::InvalidPath(_))));
        // Case-only rename of the same file is allowed.
        ix.rename("A.md", "a.md").unwrap();
        assert!(ix.entry("a.md").is_some());
    }

    #[test]
    fn plan_changes_nothing() {
        let (dir, ix) = vault(&[("A.md", ""), ("L.md", "[[A]]")]);
        let plan = ix.plan_rename("A.md", "Z.md").unwrap();
        assert_eq!(plan.edits, [LinkEdit { path: "L.md".into(), line: 0, old: "[[A]]".into(), new: "[[Z]]".into() }]);
        assert_eq!(read(&dir, "L.md"), "[[A]]");
        assert!(dir.path().join("A.md").exists());
    }

    #[test]
    fn tasks_create_append() {
        let (dir, mut ix) = vault(&[("T.md", "- [ ] one\n- [x] two\n")]);
        ix.set_task("T.md", 0, 'x').unwrap();
        ix.set_task("T.md", 1, ' ').unwrap();
        assert_eq!(read(&dir, "T.md"), "- [x] one\n- [ ] two\n");
        assert!(matches!(ix.set_task("T.md", 5, 'x'), Err(WriteError::NoTask { .. })));
        let open: Vec<_> = ix.tasks().filter(|(_, t)| t.is_open()).map(|(_, t)| t.text.clone()).collect();
        assert_eq!(open, ["two"]);

        ix.create("new/N.md", "hello [[T]]").unwrap();
        assert_eq!(ix.backlinks("T.md").len(), 1);
        assert!(matches!(ix.create("new/n.md", ""), Err(WriteError::Exists(_))));
        ix.append("new/N.md", "more").unwrap();
        assert_eq!(read(&dir, "new/N.md"), "hello [[T]]\nmore\n");
        ix.write_text("new/N.md", "replaced").unwrap();
        assert_eq!(ix.read_text("new/N.md").unwrap(), "replaced");
        assert!(ix.backlinks("T.md").is_empty());
        // No temp files left behind.
        let stray: Vec<_> = std::fs::read_dir(dir.path().join("new")).unwrap().flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.')).collect();
        assert!(stray.is_empty());
    }

    #[test]
    fn relative_paths() {
        assert_eq!(relative_path("a/b", "a/c/x.md"), "../c/x.md");
        assert_eq!(relative_path("", "x.md"), "x.md");
        assert_eq!(relative_path("a", "x.md"), "../x.md");
    }
}
