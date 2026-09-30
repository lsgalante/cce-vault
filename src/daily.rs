//! Daily notes, compatible with Obsidian's core plugin: the folder, file
//! name format and template come from `.obsidian/daily-notes.json`, and the
//! format is a moment.js format string (`YYYY-MM-DD`, `YYYY/MM/DD dddd`),
//! so a vault shared with Obsidian finds the same file for the same day.
//!
//! Templates expand the core Templates plugin's variables: `{{title}}`,
//! `{{date}}`, `{{time}}`, and `{{date:FORMAT}}` / `{{time:FORMAT}}`.

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, Timelike};
use serde::Deserialize;

use crate::index::{FileKind, Index};
use crate::write::{atomic_write, WriteError};

pub const DEFAULT_FORMAT: &str = "YYYY-MM-DD";

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DailyConfig {
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub template: String,
}

impl DailyConfig {
    /// Obsidian's settings for this vault, or its defaults (vault root,
    /// `YYYY-MM-DD`, no template) when there are none.
    pub fn load(index: &Index) -> DailyConfig {
        let path = index.root().join(".obsidian/daily-notes.json");
        let mut cfg: DailyConfig = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        cfg.folder = cfg.folder.trim().trim_matches('/').to_string();
        if cfg.format.trim().is_empty() {
            cfg.format = DEFAULT_FORMAT.to_string();
        }
        cfg
    }

    /// The vault path of `date`'s note.
    pub fn path_for(&self, date: NaiveDate) -> String {
        let name = format_moment(&date.and_hms_opt(0, 0, 0).unwrap(), &self.format);
        if self.folder.is_empty() {
            format!("{name}.md")
        } else {
            format!("{}/{name}.md", self.folder)
        }
    }
}

impl Index {
    /// `date`'s daily note: its path, and whether this call created it.
    /// With `create` unset, only the path is computed.
    pub fn daily(&mut self, date: NaiveDate, create: bool) -> Result<(String, bool), WriteError> {
        let cfg = DailyConfig::load(self);
        let path = cfg.path_for(date);
        if !create || self.entry(&path).is_some() || self.abs(&path).exists() {
            return Ok((path, false));
        }
        let body = self.template_text(&cfg.template).unwrap_or_default();
        let now = Local::now().naive_local();
        let at = date.and_hms_opt(now.hour(), now.minute(), now.second()).unwrap();
        let title = crate::index::stem(&path).to_string();
        let text = expand_template(&body, &title, &at, &cfg.format);
        atomic_write(&self.abs(&path), text.as_bytes())?;
        self.refresh(std::slice::from_ref(&path));
        Ok((path, true))
    }

    fn template_text(&self, template: &str) -> Option<String> {
        let t = template.trim().trim_start_matches('/');
        if t.is_empty() {
            return None;
        }
        let rel = if FileKind::of(t) == FileKind::Note { t.to_string() } else { format!("{t}.md") };
        std::fs::read_to_string(self.abs(&rel)).ok()
    }
}

/// Expand `{{title}}`, `{{date}}`, `{{time}}`, `{{date:FMT}}` and
/// `{{time:FMT}}`. Unknown `{{…}}` are left as written.
pub fn expand_template(body: &str, title: &str, at: &NaiveDateTime, date_format: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str(&rest[open..]);
            return out;
        };
        let inner = after[..close].trim();
        let (name, fmt) = match inner.split_once(':') {
            Some((n, f)) => (n.trim(), Some(f.trim())),
            None => (inner, None),
        };
        match name.to_ascii_lowercase().as_str() {
            "title" => out.push_str(title),
            "date" => out.push_str(&format_moment(at, fmt.unwrap_or(date_format))),
            "time" => out.push_str(&format_moment(at, fmt.unwrap_or("HH:mm"))),
            _ => out.push_str(&rest[open..open + 2 + close + 2]),
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
    "November", "December",
];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// The moment.js tokens a daily-note format uses, longest first so `MMMM`
/// wins over `MM`. `[text]` is literal, as in moment.
const TOKENS: [&str; 38] = [
    "YYYY", "GGGG", "gggg", "MMMM", "DDDD", "dddd", "MMM", "DDD", "ddd", "YY", "MM", "Mo", "DD", "Do",
    "dd", "do", "WW", "Wo", "ww", "wo", "HH", "hh", "mm", "ss", "Q", "M", "D", "d", "E", "e", "W", "w",
    "H", "h", "m", "s", "A", "a",
];

pub fn format_moment(at: &NaiveDateTime, fmt: &str) -> String {
    let d = at.date();
    let mut out = String::new();
    let mut rest = fmt;
    'outer: while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']').unwrap_or(r.len());
            out.push_str(&r[..end]);
            rest = r.get(end + 1..).unwrap_or("");
            continue;
        }
        for tok in TOKENS {
            if let Some(r) = rest.strip_prefix(tok) {
                out.push_str(&token(at, d, tok));
                rest = r;
                continue 'outer;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn token(at: &NaiveDateTime, d: NaiveDate, tok: &str) -> String {
    let dow = d.weekday().num_days_from_sunday();
    let week = d.iso_week();
    let h12 = match at.hour() % 12 {
        0 => 12,
        h => h,
    };
    match tok {
        "YYYY" => format!("{:04}", d.year()),
        "YY" => format!("{:02}", d.year().rem_euclid(100)),
        "GGGG" | "gggg" => format!("{:04}", week.year()),
        "Q" => ((d.month() - 1) / 3 + 1).to_string(),
        "MMMM" => MONTHS[d.month0() as usize].to_string(),
        "MMM" => MONTHS[d.month0() as usize][..3].to_string(),
        "MM" => format!("{:02}", d.month()),
        "Mo" => ordinal(d.month()),
        "M" => d.month().to_string(),
        "DDDD" => format!("{:03}", d.ordinal()),
        "DDD" => d.ordinal().to_string(),
        "DD" => format!("{:02}", d.day()),
        "Do" => ordinal(d.day()),
        "D" => d.day().to_string(),
        "dddd" => DAYS[dow as usize].to_string(),
        "ddd" => DAYS[dow as usize][..3].to_string(),
        "dd" => DAYS[dow as usize][..2].to_string(),
        "do" => ordinal(dow),
        "d" | "e" => dow.to_string(),
        "E" => d.weekday().number_from_monday().to_string(),
        // Locale weeks (`w`) are taken as ISO weeks, which is what an
        // en-GB or ISO-configured Obsidian shows.
        "WW" | "ww" => format!("{:02}", week.week()),
        "Wo" | "wo" => ordinal(week.week()),
        "W" | "w" => week.week().to_string(),
        "HH" => format!("{:02}", at.hour()),
        "H" => at.hour().to_string(),
        "hh" => format!("{h12:02}"),
        "h" => h12.to_string(),
        "mm" => format!("{:02}", at.minute()),
        "m" => at.minute().to_string(),
        "ss" => format!("{:02}", at.second()),
        "s" => at.second().to_string(),
        "A" => if at.hour() < 12 { "AM" } else { "PM" }.to_string(),
        "a" => if at.hour() < 12 { "am" } else { "pm" }.to_string(),
        _ => tok.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 5).unwrap()
    }

    #[test]
    fn moment_formats() {
        let t = at(2026, 9, 30, 14, 7);
        assert_eq!(format_moment(&t, "YYYY-MM-DD"), "2026-09-30");
        assert_eq!(format_moment(&t, "dddd, MMMM Do YYYY"), "Wednesday, September 30th 2026");
        assert_eq!(format_moment(&t, "YYYY/MM/YYYY-MM-DD ddd"), "2026/09/2026-09-30 Wed");
        assert_eq!(format_moment(&t, "[Week] W, gggg"), "Week 40, 2026");
        assert_eq!(format_moment(&t, "h:mm A, HH:mm:ss"), "2:07 PM, 14:07:05");
        assert_eq!(format_moment(&t, "DDDD Q E d"), "273 3 3 3");
        assert_eq!(format_moment(&at(2026, 1, 1, 0, 0), "Do MMM hh a"), "1st Jan 12 am");
        assert_eq!(format_moment(&at(2026, 1, 12, 0, 0), "Do"), "12th");
    }

    #[test]
    fn templates() {
        let t = at(2026, 9, 30, 9, 5);
        let body = "# {{title}}\nCreated {{date}} {{time}}\n{{date:dddd}} {{ time : h A }} {{other}} {{";
        assert_eq!(
            expand_template(body, "2026-09-30", &t, "YYYY-MM-DD"),
            "# 2026-09-30\nCreated 2026-09-30 09:05\nWednesday 9 AM {{other}} {{"
        );
    }

    #[test]
    fn daily_note_from_obsidian_settings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".obsidian")).unwrap();
        std::fs::write(
            root.join(".obsidian/daily-notes.json"),
            r#"{"folder":"Journal/","format":"YYYY/MM-DD ddd","template":"Templates/Day"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("Templates")).unwrap();
        std::fs::write(root.join("Templates/Day.md"), "# {{date:dddd}}\n[[{{date:YYYY-[W]WW}}]]\n").unwrap();
        let mut ix = Index::open(root, false).unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        assert_eq!(ix.daily(day, false).unwrap(), ("Journal/2026/09-30 Wed.md".to_string(), false));
        assert_eq!(ix.daily(day, true).unwrap(), ("Journal/2026/09-30 Wed.md".to_string(), true));
        assert_eq!(
            std::fs::read_to_string(root.join("Journal/2026/09-30 Wed.md")).unwrap(),
            "# Wednesday\n[[2026-W40]]\n"
        );
        assert!(!ix.daily(day, true).unwrap().1);
        // The new note's link is indexed. (The template's own
        // `[[{{date:…}}]]` is a note too, and unresolved, as in Obsidian.)
        assert!(ix.unresolved().contains_key("2026-w40"));
    }

    #[test]
    fn defaults_without_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = Index::open(dir.path(), false).unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let (path, created) = ix.daily(day, true).unwrap();
        assert_eq!((path.as_str(), created), ("2026-01-02.md", true));
        assert_eq!(std::fs::read_to_string(dir.path().join(path)).unwrap(), "");
    }
}
