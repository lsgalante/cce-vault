//! Notes about calendar events. A note names its event in its frontmatter
//! (`event: <calendar>/<event id>`), so it is found by that id however it
//! is renamed or moved; a new one is made as `Meetings/<date> <title>.md`.
//! Calendars own events; the note holds only the id, the date and what was
//! written about it.

use chrono::NaiveDate;
use serde_json::Value;

use crate::index::Index;
use crate::write::WriteError;

/// The property naming the event.
pub const PROPERTY: &str = "event";
/// Where a new event note goes.
pub const FOLDER: &str = "Meetings";

/// The note whose `event` property is `id`, if any (the first by path).
pub fn note_for(index: &Index, id: &str) -> Option<String> {
    index.files().iter().find_map(|(path, e)| {
        let p = e.note.as_ref()?.properties.get(PROPERTY)?;
        (p == &Value::String(id.to_string())).then(|| path.clone())
    })
}

/// Make the note for an event: `Meetings/<date> <title>.md` (numbered when
/// the name is taken), with the `event` and `date` properties and the title
/// as its heading. Its vault path.
pub fn create_note(index: &mut Index, id: &str, date: NaiveDate, title: &str, time: Option<&str>) -> Result<String, WriteError> {
    let clean: String = title
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '^' | '[' | ']') { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let clean = if clean.is_empty() { "Event".to_string() } else { clean };
    let stem = format!("{FOLDER}/{date} {clean}");
    let yaml_id = serde_json::to_string(id).unwrap_or_default();
    let mut body = format!("---\n{PROPERTY}: {yaml_id}\ndate: {date}\n");
    if let Some(t) = time {
        body.push_str(&format!("time: \"{t}\"\n"));
    }
    body.push_str(&format!("---\n# {title}\n\n"));
    for n in 1.. {
        let path = if n == 1 { format!("{stem}.md") } else { format!("{stem} {n}.md") };
        match index.create(&path, &body) {
            Err(WriteError::Exists(_)) => continue,
            other => return other,
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_events_note_is_made_once_and_found_by_its_id() {
        let mut ix = Index::in_memory(Vec::<(String, Vec<u8>)>::new());
        let day = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
        assert_eq!(note_for(&ix, "icloud:me/U1"), None);
        let p = create_note(&mut ix, "icloud:me/U1", day, "Studio: mix / review", Some("14:00")).unwrap();
        assert_eq!(p, "Meetings/2026-10-13 Studio mix review.md");
        assert_eq!(
            ix.read_text(&p).unwrap(),
            "---\nevent: \"icloud:me/U1\"\ndate: 2026-10-13\ntime: \"14:00\"\n---\n# Studio: mix / review\n\n"
        );
        assert_eq!(note_for(&ix, "icloud:me/U1").as_deref(), Some(p.as_str()));
        // Moved and renamed, it is still the event's note.
        ix.rename(&p, "Archive/Mix.md").unwrap();
        assert_eq!(note_for(&ix, "icloud:me/U1").as_deref(), Some("Archive/Mix.md"));
        // Another event of the same day and title gets its own note.
        let q = create_note(&mut ix, "icloud:me/U2", day, "Studio: mix / review", None).unwrap();
        let r = create_note(&mut ix, "icloud:me/U3", day, "Studio: mix / review", None).unwrap();
        assert_eq!((q.as_str(), r.as_str()), ("Meetings/2026-10-13 Studio mix review.md", "Meetings/2026-10-13 Studio mix review 2.md"));
    }
}
