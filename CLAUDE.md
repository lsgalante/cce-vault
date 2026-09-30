# cce-vault

The notes vault shared by cce apps: a folder of Markdown notes, canvases
and attachments, kept **byte-compatible with Obsidian** so the real app can
keep working on the same files. This crate is everything about that folder
that is not UI. It is milestone 1 of the Obsidian-on-cce plan; the apps
that sit on it (`cce-notes`, a vault mode in `cce-graph`, note cards on
`cce-grid`, vault tasks in `cce-list`) come later.

It has **no cce-ui dependency** on purpose. The `cce-vault` CLI, tests and
any non-GUI tool use it without a Wayland stack. There is **no daemon**:
each app embeds an `Index` and a `VaultWatcher` and applies the watcher's
batches on its own event loop. The files are the source of truth.

## Layout

| Module | What it owns |
| --- | --- |
| `parse` | One note → properties, links, tags, headings, block ids, tasks, all with byte spans |
| `index` | Every file, link resolution, backlinks, unresolved links, tags, tasks; the optional parse cache |
| `watch` | Recursive `notify` watcher, debounced (150 ms quiet, 1 s cap), hidden paths dropped |
| `search` | Fuzzy names (quick switcher), full-text scan, unlinked mentions |
| `write` | `atomic_write`, create/append, `set_task`, `rename` with link rewrite |
| `canvas` | JSON Canvas as `serde_json::Value`, written byte-exact with Obsidian |
| `daily` | Daily notes from `.obsidian/daily-notes.json`; moment.js formats; template variables |
| `config` | The vault root: `--vault`, `$CCE_VAULT`, then `vault { path "…" }` in `~/.config/cce/config.kdl` |
| `main.rs` | The `cce-vault` CLI (`cce-vault --help`) |

## Invariants — each was a design decision, keep them

- **Obsidian syntax is scanned from raw bytes; pulldown-cmark only marks
  code.** pulldown decides what is code/math and finds headings and inline
  `[text](dest)` links. Wikilinks, embeds, `#tags`, `^block` ids,
  `%%comments%%` and task statuses are scanned by hand outside those
  ranges. That gives every link an exact `target_span`, which is what a
  rename rewrites. pulldown's own `ENABLE_WIKILINKS` stays **off**: it knows
  neither `![[embed]]` nor the `\|` escape inside tables.
- **Every write re-reads and re-parses the file it edits.** The index may
  be a watcher batch behind (Obsidian or a sync client wrote a second ago),
  and a span from a stale parse cuts the wrong bytes. `set_task` and
  `rename` both do this. Do not "optimise" it into using the index's copy.
- **Resolution follows Obsidian**, case-insensitive throughout: an exact
  vault path first (relative to the note first for a *markdown* link; a
  leading `/` is always the vault root), then by file name with the link's
  folder part as a path suffix, preferring the linking note's own folder,
  then the shortest path, then alphabetical. Aliases do **not** resolve
  links, as in Obsidian. `[[Beta]]` and `[[Beta.md]]` both mean `Beta.md`;
  every other file keeps its extension (`[[pic.png]]`, `[[Board.canvas]]`).
- **Relink is whole-vault after any change batch.** Adding `Beta.md` must
  turn every unresolved `[[Beta]]` anywhere into a backlink, and it costs
  ~21 ms for 100,000 links. Don't make it incremental without a benchmark.
- **Hidden means ignored**: any path component starting with `.`
  (`.obsidian`, `.trash`, `.git`, and this crate's own `.name.PID.cce-tmp`
  write temps). The walk, `Index::rel` and the watcher all apply it.
  `.obsidian/` is read (daily-note settings) and **never written**.
- **Canvases are `Value`s with `preserve_order`, never typed structs**, and
  `canvas::to_string` reproduces Obsidian's layout (tab indent, one compact
  node per line, no trailing newline). A rename that touches a canvas must
  diff as the one field it changed. The ignored test
  `real_canvases_round_trip` checks real files:
  `CCE_CANVAS_DIR=<vault> cargo test -- --ignored`.
- **Rename writes links in the shortest form that still reaches the file**,
  and in full when the link was written in full or the short name would be
  captured by another file. `.md` suffixes, subpaths, display text and
  embed `!` are kept; markdown links stay relative or rooted as they were,
  and stay angle-bracketed or %-encoded as they were. A note moved to
  another folder gets its own relative links re-rooted, and any short
  wikilink whose target would change under the same-folder rule is pinned
  to its old target (`pin_moved_links`).

## Performance (measured 2026-09-30)

A generated vault of 5,000 notes (40 MB, 100,000 links, 7,500 tasks) on the
20-core laptop, warm page cache, release build:

| Step | Time |
| --- | --- |
| Parse (8 threads) | 36 ms |
| Relink (8 threads) | 21 ms |
| Load the JSON parse cache instead of parsing | 47 ms (18 MB file) |
| A whole CLI call (`backlinks`, `find`, `rename --dry-run`) | ~90 ms |
| `search`, `mentions` on top of the open | +15 ms, +35 ms |

So the parse cache (`Index::open(root, true)`, CLI `--cache`) is **opt-in**.
It can only pay off where reading the files is the slow part (a cold page
cache at login, a network filesystem), and that has not been measured.
Regenerate the test vault with `bench/gen-vault.py <dir>`, then time
`cce-vault --vault <dir> stats` (`RUST_LOG=debug` prints the phase split).
It lives in `bench/`, not `scripts/`: ccebuild installs every crate's
`scripts/` into `~/.local/bin`.

## Build and test

```sh
cargo test -p cce-vault                  # unit tests, incl. a real notify watcher
cargo build --release -p cce-vault
cce-vault --vault <dir> stats            # or set CCE_VAULT
```

Test writes against a **copy** of a vault (`cp -a`), never the live one:
the live vault syncs to other devices.

This crate is a workspace member and must still build standalone (its own
`Cargo.lock` is committed). Install the CLI with
`ccebuild install --no-build cce-vault` after a release build.
