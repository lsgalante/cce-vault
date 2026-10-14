//! The cce-notes instance protocol: what other apps send the running
//! cce-notes, and how they hand it a note.
//!
//! cce-notes listens on `/tmp/cce-notes-<WAYLAND_DISPLAY>.sock` (cce-core's
//! `ipc::instance`), one request line in, one reply line out. The grammar is
//! spelled here, beside the vault the notes come from, because three apps
//! send it besides cce-notes' own later launches: cce-graph, cce-desktop and
//! cce-list each kept a hand-written copy of the `open` line and its
//! fallback until 2026-10-10, and one of them waited for the reply on the UI
//! thread with no deadline.
//!
//! Requests, each answered `ok`:
//!
//! - `open <path>[#heading]` — a vault path, an absolute path inside the
//!   vault, or a note name; everything after `open ` is the target, so a
//!   name may end in a number (`Chapter 3`). A 1-based line follows a tab:
//!   `open <path>\t<line>`.
//! - `daily [YYYY-MM-DD]` — open (creating it from the template) a day's note.
//! - `search <query>` — show the search pane holding `query`.
//! - `show` — nothing but bringing the instance up.
//!
//! and two queries answered from the listener ([`Query`]): `current`
//! (`ok <vault path>` of the open note) and `vault` (`ok <vault root>`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// The instance socket's prefix: `/tmp/cce-notes-<display>.sock`.
pub const PREFIX: &str = "cce-notes";

/// How long a sender waits for cce-notes to take a request and answer it.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// The instance socket for this process's `$WAYLAND_DISPLAY`.
pub fn socket() -> String {
    cce_core::ipc::socket_path(PREFIX)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Open { target: String, line: Option<usize> },
    Daily(Option<chrono::NaiveDate>),
    Search(String),
    Show,
}

impl Command {
    pub fn to_line(&self) -> String {
        match self {
            Command::Open { target, line: Some(l) } => format!("open {target}\t{l}"),
            Command::Open { target, line: None } => format!("open {target}"),
            Command::Daily(Some(d)) => format!("daily {d}"),
            Command::Daily(None) => "daily".into(),
            Command::Search(q) => format!("search {q}"),
            Command::Show => "show".into(),
        }
    }

    pub fn parse(line: &str) -> Option<Command> {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        // `trim` would also eat the tab before a line number: only spaces.
        let rest = rest.trim_matches(' ');
        match verb {
            "open" if !rest.is_empty() => {
                // A line rides after a tab, which no note name holds. With
                // spaces only, the whole text is the target: "Chapter 3" is
                // a name, and the app sorts out an older sender's
                // `open Note 12` against the vault.
                match rest.rsplit_once('\t') {
                    Some((t, l)) if l.trim().parse::<usize>().is_ok() && !t.trim().is_empty() => {
                        Some(Command::Open { target: t.trim().to_string(), line: l.trim().parse().ok() })
                    }
                    _ => Some(Command::Open { target: rest.to_string(), line: None }),
                }
            }
            "daily" if rest.is_empty() => Some(Command::Daily(None)),
            "daily" => parse_date(rest).ok().map(|d| Command::Daily(Some(d))),
            "search" => Some(Command::Search(rest.to_string())),
            "show" => Some(Command::Show),
            _ => None,
        }
    }
}

/// `YYYY-MM-DD`.
pub fn parse_date(s: &str) -> Result<chrono::NaiveDate, String> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("not a date (YYYY-MM-DD): {s}"))
}

/// The queries the listener answers from its own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    /// The open note's vault path.
    Current,
    /// The vault root this instance shows.
    Vault,
}

impl Query {
    pub fn as_str(self) -> &'static str {
        match self {
            Query::Current => "current",
            Query::Vault => "vault",
        }
    }

    pub fn parse(line: &str) -> Option<Query> {
        match line {
            "current" => Some(Query::Current),
            "vault" => Some(Query::Vault),
            _ => None,
        }
    }

    /// The answer's value: `ok <value>` → the value; a bare `ok` (no note
    /// open, no vault) or anything else → `None`.
    pub fn parse_reply(reply: &str) -> Option<String> {
        reply.trim().strip_prefix("ok ").map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }
}

/// Send one line to the instance at `socket` and return its reply line,
/// trimmed. `None` when nothing listens, or it does not take or answer the
/// line within `timeout`.
pub fn send_at(socket: &str, line: &str, timeout: Duration) -> Option<String> {
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(timeout)).ok()?;
    s.set_write_timeout(Some(timeout)).ok()?;
    s.write_all(format!("{}\n", line.trim_end_matches('\n')).as_bytes()).ok()?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).ok()?;
    Some(reply.trim().to_string())
}

/// Ask the running instance a [`Query`]; `None` when it is not running, has
/// nothing to answer, or does not answer within `timeout`. Blocks up to
/// `timeout`: call it off the UI thread.
pub fn query(q: Query, timeout: Duration) -> Option<String> {
    Query::parse_reply(&send_at(&socket(), q.as_str(), timeout)?)
}

/// Show `target` (an absolute path, normally) in cce-notes: hand it to the
/// running instance, or start one — with `--vault <vault>` when given, so a
/// fresh instance opens the caller's vault. On a thread of its own, never on
/// the caller's: a cce-notes that takes the connection but never answers
/// must not freeze the app that asked. `on_error` gets the message when
/// cce-notes could not be started; it runs on that thread.
pub fn open<F>(target: &Path, vault: Option<&Path>, on_error: F)
where
    F: FnOnce(String) + Send + 'static,
{
    let socket = socket();
    let target = target.to_path_buf();
    let vault = vault.map(Path::to_path_buf);
    std::thread::spawn(move || {
        if let Err(e) = open_at(&socket, &target, vault.as_deref(), TIMEOUT) {
            on_error(e);
        }
    });
}

/// [`open`]'s body, on the calling thread, against an explicit socket.
pub fn open_at(socket: &str, target: &Path, vault: Option<&Path>, timeout: Duration) -> Result<(), String> {
    let cmd = Command::Open { target: target.to_string_lossy().into_owned(), line: None };
    // A connection that took the line is a delivery, answered or not: a
    // second instance started beside a slow one would only race it.
    if let Ok(mut s) = UnixStream::connect(socket) {
        let _ = s.set_read_timeout(Some(timeout));
        let _ = s.set_write_timeout(Some(timeout));
        if s.write_all(format!("{}\n", cmd.to_line()).as_bytes()).is_ok() {
            let mut reply = String::new();
            let _ = BufReader::new(s).read_line(&mut reply);
            return Ok(());
        }
    }
    let mut notes = std::process::Command::new(cce_core::process::de_bin("cce-notes"));
    if let Some(v) = vault {
        notes.arg("--vault").arg(v);
    }
    notes.arg("open").arg(target);
    cce_core::process::spawn_detached(notes).map_err(|e| format!("could not start cce-notes: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        for cmd in [
            Command::Show,
            Command::Open { target: "show".into(), line: None },
            Command::Open { target: "My Note#Sec".into(), line: Some(12) },
            Command::Daily(None),
            Command::Daily(chrono::NaiveDate::from_ymd_opt(2026, 10, 1)),
            Command::Search("two words".into()),
        ] {
            assert_eq!(Command::parse(&cmd.to_line()), Some(cmd));
        }
        assert!(Command::parse("open ").is_none());
        assert!(Command::parse("bogus").is_none());
        assert!(Command::parse("daily tomorrow").is_none());
        // A note whose name is a number is a target, not a line.
        assert_eq!(Command::parse("open 2026"), Some(Command::Open { target: "2026".into(), line: None }));
    }

    #[test]
    fn a_name_ending_in_a_number_stays_whole() {
        let open = |t: &str, l| Some(Command::Open { target: t.into(), line: l });
        assert_eq!(Command::parse("open Chapter 3"), open("Chapter 3", None));
        assert_eq!(Command::parse("open Chapter 3\t12"), open("Chapter 3", Some(12)));
        // The older space form arrives whole; the app resolves it.
        assert_eq!(Command::parse("open Note 12"), open("Note 12", None));
        assert_eq!(Command::parse("open \t5"), open("\t5", None));
    }

    #[test]
    fn query_replies() {
        assert_eq!(Query::parse_reply("ok a/N.md\n").as_deref(), Some("a/N.md"));
        assert_eq!(Query::parse_reply("ok"), None);
        assert_eq!(Query::parse_reply("error unknown command"), None);
        for q in [Query::Current, Query::Vault] {
            assert_eq!(Query::parse(q.as_str()), Some(q));
        }
    }

    #[test]
    fn opening_a_note_never_waits_long_on_a_silent_cce_notes() {
        // A listener that accepts and never answers.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("notes.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let wait = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let s = sock.to_string_lossy().into_owned();
        let t = std::thread::spawn(move || open_at(&s, Path::new("/v/N.md"), None, wait));
        let (_conn, _) = listener.accept().unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        let took = started.elapsed();
        assert!(took >= wait && took < wait * 5, "gave up after {took:?}");
    }
}
