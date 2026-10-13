//! The vault's own settings: `.cce/vault.kdl`, inside the vault so every
//! device reads the same ones (it is the one hidden file cce-sync syncs).
//!
//! ```kdl
//! daily {
//!     folder "Daily"
//!     format "YYYY-MM-DD"
//!     template "Templates/Day"
//! }
//! attachments { folder "./"; }
//! templates { folder "Templates"; }
//! ```
//!
//! Every node and field is optional; a missing or unreadable file is all
//! defaults (daily notes at the vault root named `YYYY-MM-DD`, attachments
//! at the root). `cce-sync import` writes it once from a vault's old
//! `.obsidian/` settings.

use std::path::Path;

use crate::index::Index;

/// Where the settings live, vault-relative.
pub const SETTINGS_FILE: &str = ".cce/vault.kdl";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// `daily { folder }`: where daily notes go (`""`: the vault root).
    pub daily_folder: String,
    /// `daily { format }`: the note's name, a moment.js format.
    pub daily_format: String,
    /// `daily { template }`: the note whose text a new daily note starts as.
    pub daily_template: String,
    /// `attachments { folder }`: where a new attachment goes; the forms are
    /// in [`crate::attachments`].
    pub attachments_folder: String,
    /// `templates { folder }`: the folder of note templates.
    pub templates_folder: String,
}

impl Settings {
    /// Settings from the file's text. A field that is not a string is
    /// skipped; text that is not KDL is all defaults.
    pub fn parse(text: &str) -> Settings {
        let Ok(doc) = text.parse::<kdl::KdlDocument>() else {
            log::warn!("{SETTINGS_FILE} is not valid KDL; using the defaults");
            return Settings::default();
        };
        let field = |node: &str, key: &str| {
            doc.get(node)
                .and_then(|n| n.children())
                .and_then(|c| c.get_arg(key))
                .and_then(|v| v.as_string())
                .map(str::to_string)
                .unwrap_or_default()
        };
        Settings {
            daily_folder: field("daily", "folder"),
            daily_format: field("daily", "format"),
            daily_template: field("daily", "template"),
            attachments_folder: field("attachments", "folder"),
            templates_folder: field("templates", "folder"),
        }
    }

    /// The settings of the vault `index` holds (on disk or in memory).
    pub fn load(index: &Index) -> Settings {
        index.read_text(SETTINGS_FILE).map(|t| Settings::parse(&t)).unwrap_or_default()
    }

    /// The settings of the vault folder at `root`, for callers without an
    /// [`Index`].
    pub fn read(root: &Path) -> Settings {
        std::fs::read_to_string(root.join(SETTINGS_FILE)).map(|t| Settings::parse(&t)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_and_its_absence() {
        let s = Settings::parse(
            "daily {\n    folder \"Daily\"\n    format \"YYYY-MM-DD\"\n    template \"Templates/Day\"\n}\n\
             attachments { folder \"./\"; }\ntemplates { folder \"Templates\"; }\n",
        );
        assert_eq!(
            s,
            Settings {
                daily_folder: "Daily".into(),
                daily_format: "YYYY-MM-DD".into(),
                daily_template: "Templates/Day".into(),
                attachments_folder: "./".into(),
                templates_folder: "Templates".into(),
            }
        );
        assert_eq!(Settings::parse(""), Settings::default());
        assert_eq!(Settings::parse("daily { folder 3; }"), Settings::default(), "not a string");
        assert_eq!(Settings::parse("daily {"), Settings::default(), "not KDL");
        assert_eq!(Settings::parse("attachments { folder \"A\"; }").attachments_folder, "A");
    }

    #[test]
    fn read_from_disk_and_from_memory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".cce")).unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "daily { folder \"J\"; }").unwrap();
        assert_eq!(Settings::read(dir.path()).daily_folder, "J");
        let ix = Index::open(dir.path(), false).unwrap();
        assert_eq!(Settings::load(&ix).daily_folder, "J");
        let ix = Index::in_memory([(SETTINGS_FILE, b"daily { folder \"M\"; }".to_vec())]);
        assert_eq!(Settings::load(&ix).daily_folder, "M");
        assert!(ix.files().is_empty(), "held, not indexed");
    }
}
