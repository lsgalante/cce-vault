//! Where a new attachment goes: Obsidian's "Default location for new
//! attachments" (Settings → Files and links), stored as
//! `attachmentFolderPath` in `.obsidian/app.json`, so a file another app
//! adds to the vault lands where Obsidian would have put it.
//!
//! The setting's forms, as Obsidian reads them:
//!
//! | value | folder |
//! | --- | --- |
//! | missing, `""` or `/` | the vault root (Obsidian's default) |
//! | `./` | the folder of the note (or canvas) it is for |
//! | `./sub` | `sub` inside that folder |
//! | `Some/Folder` | that folder, from the vault root |

use std::path::{Path, PathBuf};

/// The vault folder (vault-relative, `""` for the root) that a new
/// attachment for the vault file `for_file` goes in.
pub fn folder(root: &Path, for_file: &str) -> String {
    let setting = std::fs::read_to_string(root.join(".obsidian/app.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("attachmentFolderPath")?.as_str().map(str::to_string))
        .unwrap_or_default();
    folder_for(&setting, for_file)
}

/// [`folder`] for a setting already read.
pub fn folder_for(setting: &str, for_file: &str) -> String {
    let s = setting.trim();
    let here = crate::index::parent(for_file);
    let joined = match s.strip_prefix("./") {
        Some(sub) => format!("{here}/{sub}"),
        None if s == "." => here.to_string(),
        None => s.to_string(),
    };
    joined.split('/').filter(|p| !p.is_empty() && *p != ".").collect::<Vec<_>>().join("/")
}

/// An absolute path in `dir` for a file named `name` that does not exist
/// yet: `name`, else `name 1`, `name 2`, … before the extension, the way
/// Obsidian numbers a clash.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} {n}{ext}")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_forms() {
        assert_eq!(folder_for("", "Desktop.canvas"), "");
        assert_eq!(folder_for("/", "Notes/a.md"), "");
        assert_eq!(folder_for("./", "Notes/a.md"), "Notes");
        assert_eq!(folder_for("./", "a.md"), "");
        assert_eq!(folder_for("./assets", "Notes/a.md"), "Notes/assets");
        assert_eq!(folder_for("Attachments/", "Notes/a.md"), "Attachments");
        assert_eq!(folder_for("Media/Img", "a.md"), "Media/Img");
    }

    #[test]
    fn reads_app_json_and_numbers_clashes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(folder(root, "Desktop.canvas"), "");
        std::fs::create_dir_all(root.join(".obsidian")).unwrap();
        std::fs::write(root.join(".obsidian/app.json"), r#"{"attachmentFolderPath":"Attachments"}"#).unwrap();
        assert_eq!(folder(root, "Desktop.canvas"), "Attachments");
        std::fs::write(root.join("pic.png"), b"x").unwrap();
        std::fs::write(root.join("pic 1.png"), b"x").unwrap();
        assert_eq!(unique_path(root, "pic.png"), root.join("pic 2.png"));
        assert_eq!(unique_path(root, "new.png"), root.join("new.png"));
    }
}
