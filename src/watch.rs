//! A recursive watcher over the vault that hands back debounced batches
//! of changed paths, ready for [`Index::apply_changes`].
//!
//! The debounce is cce-files' shape: wait for a first event, then keep
//! collecting until 150 ms pass without another. A save from an editor is
//! several events (temp file, rename, attribute change) and a sync client
//! landing a folder is hundreds; either arrives as one batch. A stream
//! that never goes quiet (a long sync) is still delivered once a second.
//!
//! The callback runs on the watcher's own thread. A cce-ui app forwards
//! the batch to its event loop (a calloop `Sender`) and applies it there;
//! the index is not shared across threads.
//!
//! [`Index::apply_changes`]: crate::Index::apply_changes

use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

const QUIET: Duration = Duration::from_millis(150);
const MAX_WAIT: Duration = Duration::from_secs(1);

pub struct VaultWatcher {
    // Dropping the watcher closes the channel, which ends the thread.
    _watcher: RecommendedWatcher,
}

impl VaultWatcher {
    /// Watch `root` recursively. `on_change` receives each batch of
    /// absolute paths, deduplicated and sorted, with anything in a hidden
    /// folder or hidden file (`.obsidian/`, `.trash/`, this crate's own
    /// `.name.cce-tmp` files) already dropped.
    pub fn spawn<F>(root: &Path, mut on_change: F) -> notify::Result<VaultWatcher>
    where
        F: FnMut(Vec<PathBuf>) + Send + 'static,
    {
        let root = root.canonicalize().map_err(notify::Error::io)?;
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            match res {
                Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                    for p in event.paths {
                        let _ = tx.send(p);
                    }
                }
                Ok(_) => {}
                Err(e) => log::warn!("vault watcher: {e}"),
            }
        })?;
        watcher.watch(&root, RecursiveMode::Recursive)?;

        let filter_root = root.clone();
        std::thread::Builder::new()
            .name("cce-vault-watch".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let mut batch = vec![first];
                    let started = std::time::Instant::now();
                    while let Some(left) = MAX_WAIT.checked_sub(started.elapsed()) {
                        match rx.recv_timeout(QUIET.min(left)) {
                            Ok(p) => batch.push(p),
                            Err(_) => break,
                        }
                    }
                    batch.retain(|p| visible(&filter_root, p));
                    batch.sort();
                    batch.dedup();
                    if !batch.is_empty() {
                        on_change(batch);
                    }
                }
            })
            .map_err(notify::Error::io)?;
        Ok(VaultWatcher { _watcher: watcher })
    }
}

fn visible(root: &Path, path: &Path) -> bool {
    match path.strip_prefix(root) {
        Ok(rel) => rel.components().all(|c| match c {
            Component::Normal(s) => !s.to_string_lossy().starts_with('.'),
            _ => true,
        }),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    #[test]
    fn batches_arrive_debounced_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let seen: Arc<Mutex<Vec<Vec<PathBuf>>>> = Arc::default();
        let sink = seen.clone();
        let _w = VaultWatcher::spawn(&root, move |b| sink.lock().unwrap().push(b)).unwrap();

        std::fs::create_dir_all(root.join(".obsidian")).unwrap();
        std::fs::write(root.join(".obsidian/app.json"), "{}").unwrap();
        crate::write::atomic_write(&root.join("sub/Note.md"), b"hello").unwrap();
        std::fs::write(root.join("Other.md"), "x").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let all: Vec<PathBuf> = seen.lock().unwrap().iter().flatten().cloned().collect();
            // A file written into a brand-new folder can land before the
            // watch on that folder exists; the folder's own event covers it
            // (apply_changes walks a folder it is handed).
            let sub = all.contains(&root.join("sub/Note.md")) || all.contains(&root.join("sub"));
            if all.contains(&root.join("Other.md")) && sub {
                assert!(all.iter().all(|p| visible(&root, p)), "{all:?}");
                break;
            }
            assert!(Instant::now() < deadline, "no batch within 5 s: {all:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
