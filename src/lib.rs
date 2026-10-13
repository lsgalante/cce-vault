//! The notes vault shared by cce apps.
//!
//! A vault is a folder of Markdown notes (with wikilinks, embeds, tags and
//! frontmatter), JSON Canvas boards and attachments, plus its settings in
//! `.cce/vault.kdl`.
//! This crate is everything about that folder that is not UI:
//!
//! - [`parse`]: one note's properties, links, tags, headings, block ids
//!   and tasks, with exact byte spans.
//! - [`index`]: every file, where each link resolves, backlinks, tags and
//!   tasks; cached on disk by mtime and patched as files change.
//! - [`watch`]: a recursive, debounced watcher feeding the index.
//! - [`search`]: fuzzy name matching, full-text search, unlinked mentions.
//! - [`write`]: atomic writes, task toggling, and rename with link rewrite.
//! - [`canvas`]: JSON Canvas read and write, byte-exact with the files.
//! - [`daily`]: daily notes from the vault's settings and templates.
//! - [`settings`]: the vault's settings, `.cce/vault.kdl`.
//! - [`events`]: notes about calendar events (`event:` in frontmatter).
//! - [`agenda`]: a note's ```` ```agenda ```` blocks, filled when drawn.
//! - [`markdown`]: a note as styled blocks and spans, for reading views.
//! - [`config`]: where the vault is (`CCE_VAULT`, or `vault { path }` in
//!   `~/.config/cce/config.kdl`).
//!
//! It has no cce-ui dependency on purpose: the `cce-vault` CLI, tests and
//! any future non-GUI tool use it without a Wayland stack. There is no
//! daemon; each app embeds an [`Index`] and a [`VaultWatcher`].
//!
//! ```no_run
//! let root = cce_vault::config::vault_root(None)?;
//! let mut index = cce_vault::Index::open(&root, true)?;
//! for b in index.backlinks("Projects/cce.md") {
//!     println!("{}:{}", b.source, b.link.line + 1);
//! }
//! index.save_cache()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod agenda;
pub mod attachments;
pub mod canvas;
pub mod config;
pub mod daily;
pub mod events;
pub mod index;
/// A note as a reading view needs it, and the leaf rules of the vault's
/// Markdown: `cce_core::markdown`, shared with cce-ui's `MarkdownView` and
/// `DocEditor`, re-exported here at its old path.
pub use cce_core::markdown;
#[cfg(unix)]
pub mod notes_ipc;
pub mod parse;
pub mod search;
pub mod settings;
pub mod watch;
pub mod write;

pub use index::{Backlink, Changes, Entry, FileKind, Index};
pub use parse::{split_due, with_due, Link, LinkKind, Note, Task, DUE_MARK};
pub use watch::VaultWatcher;
pub use write::{LinkEdit, RenamePlan, TaskPlace, WriteError};
