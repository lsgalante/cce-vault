//! The vault index: every file, every parsed note, and where each link
//! points.
//!
//! The files are the source of truth. The index is rebuilt from them on
//! [`Index::open`], reusing a parse from the on-disk cache only where a
//! file's size and mtime still match, and patched with
//! [`Index::apply_changes`] as the watcher reports paths. There is no daemon;
//! every app that needs the index holds its own.
//!
//! Resolution is re-run for the whole vault after any change batch. That
//! keeps the one subtle case correct for free: creating `Beta.md` must turn
//! every `[[Beta]]` that was unresolved anywhere into a backlink, and
//! deleting it must turn them back.
//!
//! Measured on a generated vault of 5,000 notes (40 MB, 100,000 links,
//! 20-core laptop, warm page cache, 2026-09-30): parse 36 ms and relink
//! 21 ms, both spread over up to eight threads; loading the JSON cache
//! instead takes 47 ms for an 18 MB file. So the cache is opt-in. It can
//! only pay off where reading the files is the slow part (a cold page cache
//! at login, a network filesystem), which has not been measured yet.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::canvas;
use crate::parse::{self, Link, LinkKind, Note, Task};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Note,
    Canvas,
    Attachment,
}

impl FileKind {
    pub fn of(path: &str) -> FileKind {
        match extension(path).map(str::to_ascii_lowercase).as_deref() {
            Some("md") => FileKind::Note,
            Some("canvas") => FileKind::Canvas,
            _ => FileKind::Attachment,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub kind: FileKind,
    /// Nanoseconds since the epoch; with `size`, the cache's validity key.
    pub mtime: u64,
    pub size: u64,
    /// Parsed content for notes and canvases; `None` for attachments.
    pub note: Option<Note>,
}

/// How a build went, for `cce-vault stats` and for noticing a cold cache.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BuildStats {
    pub parsed: usize,
    pub reused: usize,
    pub millis: u128,
}

/// What [`Index::apply_changes`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Changes {
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.updated.is_empty() && self.removed.is_empty()
    }
}

/// A link seen from its target: which file it is in, and the link itself.
#[derive(Debug, Clone, Copy)]
pub struct Backlink<'a> {
    pub source: &'a str,
    pub link: &'a Link,
}

pub struct Index {
    root: PathBuf,
    files: BTreeMap<String, Entry>,
    /// Lowercased basename → paths. Notes are keyed without `.md`, since
    /// `[[Beta]]` and `[[Beta.md]]` both mean `Beta.md`; every other file
    /// keeps its extension (`[[pic.png]]`, `[[Board.canvas]]`).
    by_name: HashMap<String, Vec<String>>,
    /// Lowercased path → path, for case-insensitive exact matches.
    by_lower: HashMap<String, String>,
    /// Per source file, where each of its links resolved, in link order.
    resolved: HashMap<String, Vec<Option<String>>>,
    /// Target path → (source path, link index).
    backlinks: HashMap<String, Vec<(String, usize)>>,
    cache_dirty: bool,
    pub stats: BuildStats,
}

impl Index {
    /// Index the vault at `root`, reusing the on-disk cache where it is
    /// still valid when `use_cache` is set.
    pub fn open(root: &Path, use_cache: bool) -> io::Result<Index> {
        let started = Instant::now();
        let root = root.canonicalize()?;
        let mut cached = if use_cache { load_cache(&root) } else { BTreeMap::new() };
        let t_cache = started.elapsed();
        let mut index = Index::empty(root.clone());
        let found = walk(&root, &root)?;
        let found_count = found.len();
        let mut to_parse: Vec<(String, PathBuf, u64, u64)> = Vec::new();
        for (rel, abs) in found {
            let Ok(meta) = std::fs::metadata(&abs) else { continue };
            let (mtime, size) = stamp(&meta);
            match cached.remove(&rel) {
                Some(entry) if entry.mtime == mtime && entry.size == size => {
                    index.stats.reused += 1;
                    index.files.insert(rel, entry);
                }
                _ => to_parse.push((rel, abs, mtime, size)),
            }
        }
        index.stats.parsed = to_parse.len();
        let parsed = par_map(&to_parse, |(rel, abs, mtime, size)| {
            Some((rel.clone(), read_entry(abs, rel, *mtime, *size)))
        });
        index.files.extend(parsed);
        let t_files = started.elapsed();        // Anything the cache knew that the walk did not find is gone.
        index.cache_dirty = use_cache && (index.stats.parsed > 0 || !cached.is_empty());
        index.relink();
        index.stats.millis = started.elapsed().as_millis();
        log::debug!(
            "indexed {} files under {} ({} parsed, {} from cache) in {} ms \
             (cache load {} ms, walk+parse {} ms, relink {} ms)",
            found_count,
            root.display(),
            index.stats.parsed,
            index.stats.reused,
            index.stats.millis,
            t_cache.as_millis(),
            (t_files - t_cache).as_millis(),
            (started.elapsed() - t_files).as_millis(),
        );
        Ok(index)
    }

    fn empty(root: PathBuf) -> Index {
        Index {
            root,
            files: BTreeMap::new(),
            by_name: HashMap::new(),
            by_lower: HashMap::new(),
            resolved: HashMap::new(),
            backlinks: HashMap::new(),
            cache_dirty: false,
            stats: BuildStats::default(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The vault-relative path of an absolute one, or `None` when it is
    /// outside the vault or inside a hidden folder (`.obsidian`, `.trash`).
    pub fn rel(&self, abs: &Path) -> Option<String> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        let mut parts = Vec::new();
        for c in rel.components() {
            let Component::Normal(s) = c else { return None };
            let s = s.to_str()?;
            if s.starts_with('.') {
                return None;
            }
            parts.push(s);
        }
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    pub fn files(&self) -> &BTreeMap<String, Entry> {
        &self.files
    }

    pub fn entry(&self, path: &str) -> Option<&Entry> {
        self.files.get(path)
    }

    pub fn note(&self, path: &str) -> Option<&Note> {
        self.files.get(path)?.note.as_ref()
    }

    /// Markdown notes, in path order.
    pub fn notes(&self) -> impl Iterator<Item = (&str, &Note)> {
        self.documents().filter(|(p, _)| FileKind::of(p) == FileKind::Note)
    }

    /// Notes and canvases: every file that can hold links.
    pub fn documents(&self) -> impl Iterator<Item = (&str, &Note)> {
        self.files.iter().filter_map(|(p, e)| Some((p.as_str(), e.note.as_ref()?)))
    }

    /// Re-read the files at these absolute paths (as a watcher reports
    /// them): changed ones are re-parsed, vanished ones dropped, a directory
    /// that appeared is walked. Paths outside the vault or in hidden folders
    /// are ignored.
    pub fn apply_changes(&mut self, paths: &[PathBuf]) -> Changes {
        let mut changes = Changes::default();
        for abs in paths {
            let Some(rel) = self.rel(abs) else { continue };
            match std::fs::metadata(abs) {
                Ok(meta) if meta.is_dir() => {
                    if let Ok(found) = walk(&self.root, abs) {
                        for (rel, abs) in found {
                            self.upsert(&rel, &abs, &mut changes);
                        }
                    }
                }
                Ok(_) => self.upsert(&rel, abs, &mut changes),
                Err(_) => {
                    // A file, or a directory that took files with it.
                    let prefix = format!("{rel}/");
                    let gone: Vec<String> = self
                        .files
                        .keys()
                        .filter(|k| **k == rel || k.starts_with(&prefix))
                        .cloned()
                        .collect();
                    for k in gone {
                        self.files.remove(&k);
                        changes.removed.push(k);
                    }
                }
            }
        }
        if !changes.is_empty() {
            changes.updated.sort();
            changes.updated.dedup();
            changes.removed.sort();
            changes.removed.dedup();
            self.cache_dirty = true;
            self.relink();
        }
        changes
    }

    fn upsert(&mut self, rel: &str, abs: &Path, changes: &mut Changes) {
        let Ok(meta) = std::fs::metadata(abs) else { return };
        let (mtime, size) = stamp(&meta);
        if let Some(e) = self.files.get(rel) {
            if e.mtime == mtime && e.size == size {
                return;
            }
        }
        self.files.insert(rel.to_string(), read_entry(abs, rel, mtime, size));
        changes.updated.push(rel.to_string());
    }

    /// Rebuild the name maps, every link's resolution and the backlinks.
    fn relink(&mut self) {
        self.by_name.clear();
        self.by_lower.clear();
        for path in self.files.keys() {
            self.by_name.entry(name_key(path)).or_default().push(path.clone());
            self.by_lower.insert(path.to_lowercase(), path.clone());
        }
        // Resolution only reads the maps, so it spreads across threads.
        let docs: Vec<(&str, &Note)> = self.documents().collect();
        let per_doc = par_map(&docs, |(source, note)| {
            let targets: Vec<Option<String>> =
                note.links.iter().map(|l| self.resolve(Some(source), l)).collect();
            Some((source.to_string(), targets))
        });
        let mut resolved = HashMap::new();
        let mut backlinks: HashMap<String, Vec<(String, usize)>> = HashMap::new();
        for (source, targets) in per_doc {
            for (i, t) in targets.iter().enumerate() {
                if let Some(t) = t {
                    backlinks.entry(t.clone()).or_default().push((source.clone(), i));
                }
            }
            resolved.insert(source, targets);
        }
        self.resolved = resolved;
        self.backlinks = backlinks;
    }

    /// Where a link in `from` points, following Obsidian's rules: an exact
    /// vault path first (relative to the note for a markdown link), then
    /// the file whose name matches, preferring one in the linking note's
    /// own folder and then the shortest path. Case-insensitive throughout,
    /// as Obsidian is.
    pub fn resolve(&self, from: Option<&str>, link: &Link) -> Option<String> {
        let relative_first = link.kind == LinkKind::Markdown;
        self.resolve_path(from, &link.target, relative_first)
    }

    /// Resolve link text as a user would type it (`Note`, `folder/Note`,
    /// `Note#Heading`); the subpath is ignored.
    pub fn resolve_text(&self, from: Option<&str>, text: &str) -> Option<String> {
        let path = text.split('#').next().unwrap_or(text).trim();
        self.resolve_path(from, path, false)
    }

    fn resolve_path(&self, from: Option<&str>, target: &str, relative_first: bool) -> Option<String> {
        let target = target.trim();
        if target.is_empty() {
            return None;
        }
        let from_dir = from.map(parent).unwrap_or("");
        let explicit_rel = target.starts_with("./") || target.starts_with("../");
        // A leading `/` means the vault root, never the note's folder.
        let rooted_only = target.starts_with('/');
        let mut candidates = vec![target.to_string()];
        if FileKind::of(target) != FileKind::Note {
            candidates.push(format!("{target}.md"));
        }
        for c in &candidates {
            let rooted = normalize("", c.trim_start_matches('/'));
            let relative = normalize(from_dir, c);
            let relative = if rooted_only { None } else { relative };
            let order = if relative_first || explicit_rel {
                [relative, rooted]
            } else {
                [rooted, relative]
            };
            for p in order.into_iter().flatten() {
                if let Some(hit) = self.by_lower.get(&p.to_lowercase()) {
                    return Some(hit.clone());
                }
            }
        }
        if explicit_rel {
            return None;
        }
        // By name, with any folder part of the link as a path suffix.
        let clean = target.trim_start_matches('/');
        let lower = clean.to_lowercase();
        let lower_md = format!("{lower}.md");
        let mut hits: Vec<&String> = self
            .by_name
            .get(&name_key(clean))?
            .iter()
            .filter(|p| {
                let p = p.to_lowercase();
                [&lower, &lower_md].iter().any(|t| {
                    p == **t || (p.ends_with(t.as_str()) && p[..p.len() - t.len()].ends_with('/'))
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            let same_a = parent(a) == from_dir;
            let same_b = parent(b) == from_dir;
            same_b.cmp(&same_a).then(a.len().cmp(&b.len())).then(a.cmp(b))
        });
        hits.first().map(|p| (*p).clone())
    }

    /// Each outgoing link of `path` with the file it resolves to.
    pub fn outgoing(&self, path: &str) -> Vec<(&Link, Option<&str>)> {
        let Some(note) = self.note(path) else { return Vec::new() };
        let resolved = self.resolved.get(path);
        note.links
            .iter()
            .enumerate()
            .map(|(i, l)| (l, resolved.and_then(|r| r.get(i)?.as_deref())))
            .collect()
    }

    /// Every link that points at `path`, in source-path then line order.
    pub fn backlinks(&self, path: &str) -> Vec<Backlink<'_>> {
        let mut out: Vec<Backlink> = self
            .backlinks
            .get(path)
            .into_iter()
            .flatten()
            .filter_map(|(source, i)| {
                let (source, entry) = self.files.get_key_value(source)?;
                Some(Backlink { source, link: entry.note.as_ref()?.links.get(*i)? })
            })
            .collect();
        out.sort_by(|a, b| a.source.cmp(b.source).then(a.link.span.start.cmp(&b.link.span.start)));
        out
    }

    /// Links that resolve to nothing, grouped by what they ask for
    /// (lowercased, the way Obsidian merges `[[idea]]` and `[[Idea]]`).
    pub fn unresolved(&self) -> BTreeMap<String, Vec<Backlink<'_>>> {
        let mut out: BTreeMap<String, Vec<Backlink>> = BTreeMap::new();
        for (source, note) in self.documents() {
            let Some(resolved) = self.resolved.get(source) else { continue };
            for (link, target) in note.links.iter().zip(resolved) {
                if target.is_none() {
                    out.entry(link.target.to_lowercase())
                        .or_default()
                        .push(Backlink { source, link });
                }
            }
        }
        out
    }

    /// Tag → number of notes carrying it. Tags are case-insensitive; each
    /// is shown in the casing first met. Nested tags count toward their
    /// parents too (`#a/b` is also `#a`), as Obsidian's tag pane shows them.
    pub fn tags(&self) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<String, (String, usize)> = BTreeMap::new();
        for (_, note) in self.documents() {
            let mut seen = std::collections::HashSet::new();
            for tag in &note.tags {
                let parts: Vec<&str> = tag.name.split('/').collect();
                for n in 1..=parts.len() {
                    let name = parts[..n].join("/");
                    if seen.insert(name.to_lowercase()) {
                        let e = counts.entry(name.to_lowercase()).or_insert((name, 0));
                        e.1 += 1;
                    }
                }
            }
        }
        counts.into_values().collect()
    }

    /// Documents carrying `tag` or one nested under it.
    pub fn tagged(&self, tag: &str) -> Vec<&str> {
        let want = tag.trim_start_matches('#').to_lowercase();
        let nested = format!("{want}/");
        self.documents()
            .filter(|(_, n)| {
                n.tags.iter().any(|t| {
                    let t = t.name.to_lowercase();
                    t == want || t.starts_with(&nested)
                })
            })
            .map(|(p, _)| p)
            .collect()
    }

    /// Every task in the vault, in path then line order.
    pub fn tasks(&self) -> impl Iterator<Item = (&str, &Task)> {
        self.documents().flat_map(|(p, n)| n.tasks.iter().map(move |t| (p, t)))
    }

    /// A note named on a command line or in a request: an exact path, a
    /// path missing its `.md`, or link text resolved from the vault root.
    pub fn lookup(&self, query: &str) -> Option<String> {
        let q = query.trim().trim_start_matches('/');
        if self.files.contains_key(q) {
            return Some(q.to_string());
        }
        self.resolve_text(None, q)
    }

    /// Write the cache if anything changed since it was read.
    pub fn save_cache(&mut self) -> io::Result<()> {
        if !self.cache_dirty {
            return Ok(());
        }
        let path = cache_path(&self.root);
        let file = CacheFile {
            version: CACHE_VERSION,
            root: self.root.to_string_lossy().into_owned(),
            files: self.files.clone(),
        };
        let bytes = serde_json::to_vec(&file).map_err(io::Error::other)?;
        crate::write::atomic_write(&path, &bytes)?;
        self.cache_dirty = false;
        Ok(())
    }

    /// Record a file this process just wrote, so the watcher's echo of the
    /// write is recognised as already applied.
    pub(crate) fn refresh(&mut self, rels: &[String]) -> Changes {
        let paths: Vec<PathBuf> = rels.iter().map(|r| self.abs(r)).collect();
        self.apply_changes(&paths)
    }
}

/// Bumped whenever `Entry` or `Note` change shape; an old cache is then
/// ignored rather than misread.
const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    root: String,
    files: BTreeMap<String, Entry>,
}

fn cache_path(root: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    // FNV-1a: std's hasher is not stable across Rust releases, and the
    // cache file name must be.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root.to_string_lossy().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    base.join("cce").join("vault").join(format!("{h:016x}.json"))
}

fn load_cache(root: &Path) -> BTreeMap<String, Entry> {
    let Ok(bytes) = std::fs::read(cache_path(root)) else { return BTreeMap::new() };
    match serde_json::from_slice::<CacheFile>(&bytes) {
        Ok(c) if c.version == CACHE_VERSION && Path::new(&c.root) == root => c.files,
        Ok(_) => BTreeMap::new(),
        Err(e) => {
            log::warn!("ignoring unreadable vault cache: {e}");
            BTreeMap::new()
        }
    }
}

/// Run `f` over `items` on up to eight threads, keeping the `Some`
/// results. Small inputs stay on the calling thread.
pub(crate) fn par_map<I, T, F>(items: &[I], f: F) -> Vec<T>
where
    I: Sync,
    T: Send,
    F: Fn(&I) -> Option<T> + Sync,
{
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    if items.len() < 64 || threads < 2 {
        return items.iter().filter_map(&f).collect();
    }
    let chunk = items.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| {
                let f = &f;
                s.spawn(move || part.iter().filter_map(f).collect::<Vec<T>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

fn stamp(meta: &std::fs::Metadata) -> (u64, u64) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    (mtime, meta.len())
}

fn read_entry(abs: &Path, rel: &str, mtime: u64, size: u64) -> Entry {
    let kind = FileKind::of(rel);
    let note = match kind {
        FileKind::Attachment => None,
        _ => {
            let text = match std::fs::read(abs) {
                Ok(bytes) => String::from_utf8(bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
                Err(e) => {
                    log::warn!("cannot read {}: {e}", abs.display());
                    String::new()
                }
            };
            Some(match kind {
                FileKind::Canvas => match canvas::from_str(&text) {
                    Ok(c) => canvas::index(&c),
                    Err(e) => {
                        log::warn!("{rel}: {e}");
                        Note::default()
                    }
                },
                _ => parse::parse(&text),
            })
        }
    };
    Entry { kind, mtime, size, note }
}

/// Every visible file under `dir`, as (vault-relative path, absolute path).
/// Hidden files and folders (`.obsidian`, `.trash`, `.git`) are skipped the
/// way Obsidian skips them. Symlinks are followed; walkdir breaks loops.
fn walk(root: &Path, dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::warn!("walking the vault: {e}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let Some(rel) = rel.to_str() else { continue };
        out.push((rel.replace('\\', "/"), entry.path().to_path_buf()));
    }
    Ok(out)
}

fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    let dot = name.rfind('.')?;
    (dot > 0).then(|| &name[dot + 1..])
}

/// The folder part of a vault path (`""` at the root).
pub fn parent(path: &str) -> &str {
    path.rfind('/').map(|i| &path[..i]).unwrap_or("")
}

/// A note's display name: its file name without `.md`.
pub fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    if FileKind::of(name) == FileKind::Note {
        &name[..name.len() - 3]
    } else {
        name
    }
}

fn name_key(path: &str) -> String {
    stem(path.trim_end_matches('/')).to_lowercase()
}

/// Join `rel` onto `dir` and fold `.` and `..`; `None` if it climbs out of
/// the vault.
fn normalize(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
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

    fn resolve(ix: &Index, from: &str, text: &str) -> Option<String> {
        ix.resolve_text(Some(from), text)
    }

    #[test]
    fn resolution_rules() {
        let (_d, ix) = vault(&[
            ("Alpha.md", ""),
            ("a/Dup.md", ""),
            ("b/Dup.md", ""),
            ("b/deep/Dup.md", ""),
            ("a/Only.md", ""),
            ("img/pic.png", ""),
            ("Board.canvas", "{\"nodes\":[],\"edges\":[]}"),
            (".obsidian/app.json", "{}"),
            ("b/Linker.md", ""),
        ]);
        assert_eq!(resolve(&ix, "b/Linker.md", "alpha").as_deref(), Some("Alpha.md"));
        assert_eq!(resolve(&ix, "b/Linker.md", "Alpha.md").as_deref(), Some("Alpha.md"));
        assert_eq!(resolve(&ix, "b/Linker.md", "Only#Part").as_deref(), Some("a/Only.md"));
        // Same folder wins, then the shortest path.
        assert_eq!(resolve(&ix, "b/Linker.md", "Dup").as_deref(), Some("b/Dup.md"));
        assert_eq!(resolve(&ix, "Alpha.md", "Dup").as_deref(), Some("a/Dup.md"));
        // A folder part narrows by path suffix.
        assert_eq!(resolve(&ix, "Alpha.md", "deep/Dup").as_deref(), Some("b/deep/Dup.md"));
        assert_eq!(resolve(&ix, "Alpha.md", "b/Dup").as_deref(), Some("b/Dup.md"));
        assert_eq!(resolve(&ix, "Alpha.md", "pic.png").as_deref(), Some("img/pic.png"));
        assert_eq!(resolve(&ix, "Alpha.md", "Board.canvas").as_deref(), Some("Board.canvas"));
        assert_eq!(resolve(&ix, "Alpha.md", "Board"), None);
        assert_eq!(resolve(&ix, "Alpha.md", "Missing"), None);
        assert!(ix.files().keys().all(|k| !k.starts_with('.')));
    }

    #[test]
    fn markdown_links_resolve_relative_first() {
        let (_d, ix) = vault(&[
            ("x.md", "root"),
            ("sub/x.md", "sub"),
            ("sub/n.md", "[r](x.md) [up](../x.md) [abs](/x.md) [sp](My%20File.md)"),
            ("sub/My File.md", ""),
        ]);
        let out: Vec<_> = ix.outgoing("sub/n.md").into_iter().map(|(_, t)| t).collect();
        assert_eq!(out, [Some("sub/x.md"), Some("x.md"), Some("x.md"), Some("sub/My File.md")]);
    }

    #[test]
    fn backlinks_unresolved_tags_tasks() {
        let (_d, ix) = vault(&[
            ("A.md", "[[B]] [[B#Sec|b]] [[Nope]] #proj/x\n- [ ] one\n- [x] two\n"),
            ("B.md", "---\ntags: [proj]\n---\n[[A]] [[nope]]\n"),
            ("C.canvas", "{\"nodes\":[{\"id\":\"n\",\"type\":\"file\",\"file\":\"B.md\",\"x\":0,\"y\":0,\"width\":1,\"height\":1}],\"edges\":[]}"),
        ]);
        let bl: Vec<_> = ix.backlinks("B.md").iter().map(|b| (b.source, b.link.line)).collect();
        assert_eq!(bl, [("A.md", 0), ("A.md", 0), ("C.canvas", 0)]);
        let un = ix.unresolved();
        assert_eq!(un.keys().collect::<Vec<_>>(), ["nope"]);
        assert_eq!(un["nope"].len(), 2);
        assert_eq!(ix.tags(), [("proj".to_string(), 2), ("proj/x".to_string(), 1)]);
        assert_eq!(ix.tagged("#proj"), ["A.md", "B.md"]);
        let open: Vec<_> = ix.tasks().filter(|(_, t)| t.is_open()).map(|(p, t)| (p, t.text.as_str())).collect();
        assert_eq!(open, [("A.md", "one")]);
    }

    #[test]
    fn changes_relink_the_vault() {
        let (dir, mut ix) = vault(&[("A.md", "[[B]]"), ("old/C.md", "[[A]]")]);
        assert!(ix.backlinks("B.md").is_empty());
        std::fs::write(dir.path().join("B.md"), "[[A]]").unwrap();
        let ch = ix.apply_changes(&[dir.path().join("B.md")]);
        assert_eq!(ch.updated, ["B.md"]);
        assert_eq!(ix.backlinks("B.md").len(), 1);
        assert_eq!(ix.backlinks("A.md").len(), 2);

        std::fs::remove_dir_all(dir.path().join("old")).unwrap();
        let ch = ix.apply_changes(&[dir.path().join("old")]);
        assert_eq!(ch.removed, ["old/C.md"]);
        assert_eq!(ix.backlinks("A.md").len(), 1);

        std::fs::create_dir_all(dir.path().join("new/deeper")).unwrap();
        std::fs::write(dir.path().join("new/deeper/D.md"), "[[B]]").unwrap();
        let ch = ix.apply_changes(&[dir.path().join("new")]);
        assert_eq!(ch.updated, ["new/deeper/D.md"]);
        assert_eq!(ix.backlinks("B.md").len(), 2);

        // Unchanged files and hidden paths are no-ops.
        let ch = ix.apply_changes(&[dir.path().join("A.md"), dir.path().join(".obsidian/x.json")]);
        assert!(ch.is_empty());
    }

    #[test]
    fn cache_is_reused_until_a_file_changes() {
        let cache = tempfile::tempdir().unwrap();
        // The only test that reads XDG_CACHE_HOME; every other index test
        // opens with the cache off.
        std::env::set_var("XDG_CACHE_HOME", cache.path());
        let (dir, _) = vault(&[("A.md", "[[B]]"), ("B.md", "x")]);
        let mut first = Index::open(dir.path(), true).unwrap();
        assert_eq!((first.stats.parsed, first.stats.reused), (2, 0));
        first.save_cache().unwrap();
        let second = Index::open(dir.path(), true).unwrap();
        assert_eq!((second.stats.parsed, second.stats.reused), (0, 2));
        assert_eq!(second.backlinks("B.md").len(), 1);
        std::fs::write(dir.path().join("B.md"), "changed, and longer").unwrap();
        let third = Index::open(dir.path(), true).unwrap();
        assert_eq!((third.stats.parsed, third.stats.reused), (1, 1));
    }

    #[test]
    fn helpers() {
        assert_eq!(normalize("a/b", "../c.md").as_deref(), Some("a/c.md"));
        assert_eq!(normalize("", "../c.md"), None);
        assert_eq!(stem("x/Note.md"), "Note");
        assert_eq!(stem("x/pic.png"), "pic.png");
        assert_eq!(FileKind::of("a/B.MD"), FileKind::Note);
        assert_eq!(FileKind::of(".md"), FileKind::Attachment);
    }
}
