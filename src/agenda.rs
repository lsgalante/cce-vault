//! A note's agenda blocks: an empty fenced block whose language is
//! `agenda` stands for a day's calendar (cce-sync's Google bridge fills it
//! when the note is drawn; the file keeps the block empty, so it stays plain
//! Markdown and nothing from the calendar is ever written into a note).
//!
//! The day is the date written inside the block, if any, else the note's
//! own day when it is a daily note ([`crate::daily::DailyConfig::date_of`]).
//! Reading views call [`fill`] on the parsed blocks, after parsing, so no
//! other block's line moves.

use chrono::NaiveDate;

use crate::markdown::Block;

/// The fenced block's language.
pub const LANG: &str = "agenda";

/// The day each agenda block in `blocks` asks for, in order (top level and
/// inside quotes). `note_day` is the note's own day, for empty blocks.
pub fn wanted(blocks: &[Block], note_day: Option<NaiveDate>) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    visit(blocks, &mut |lang, text| {
        if lang == Some(LANG) {
            if let Some(d) = day(text, note_day) {
                if !out.contains(&d) {
                    out.push(d);
                }
            }
        }
    });
    out
}

/// Put each agenda block's lines in its text: `lines(day)`, or `pending`
/// while they are not known. A block with no day (an empty one in a note
/// that is not a daily note) says so.
pub fn fill(blocks: &mut [Block], note_day: Option<NaiveDate>, lines: &dyn Fn(NaiveDate) -> Option<Vec<String>>, pending: &str) {
    for b in blocks.iter_mut() {
        match b {
            Block::Code { lang, text, .. } if lang.as_deref() == Some(LANG) => {
                *text = match day(text, note_day) {
                    Some(d) => lines(d).map(|l| l.join("\n")).unwrap_or_else(|| pending.to_string()),
                    None => "(no day: write a date, YYYY-MM-DD, in the block)".to_string(),
                };
            }
            Block::Quote { blocks, .. } => fill(blocks, note_day, lines, pending),
            _ => {}
        }
    }
}

fn day(text: &str, note_day: Option<NaiveDate>) -> Option<NaiveDate> {
    match text.trim() {
        "" => note_day,
        t => NaiveDate::parse_from_str(t, "%Y-%m-%d").ok(),
    }
}

fn visit<'a>(blocks: &'a [Block], f: &mut dyn FnMut(Option<&'a str>, &'a str)) {
    for b in blocks {
        match b {
            Block::Code { lang, text, .. } => f(lang.as_deref(), text),
            Block::Quote { blocks, .. } => visit(blocks, f),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::blocks;

    #[test]
    fn blocks_are_filled_in_place_without_moving_lines() {
        let src = "# Day\n\n```agenda\n```\n\n- [ ] after\n\n> ```agenda\n> 2026-10-20\n> ```\n\n```rust\nfn x() {}\n```\n";
        let mut b = blocks(src);
        let today = NaiveDate::from_ymd_opt(2026, 10, 13);
        assert_eq!(wanted(&b, today), vec![today.unwrap(), NaiveDate::from_ymd_opt(2026, 10, 20).unwrap()]);
        let lines_before: Vec<String> = b.iter().map(|x| format!("{x:?}")).filter(|s| s.contains("line")).collect();
        fill(&mut b, today, &|d| (d.to_string() == "2026-10-13").then(|| vec!["09:00–09:15  Standup".to_string()]), "Loading…");
        let dbg = format!("{b:?}");
        assert!(dbg.contains("09:00–09:15  Standup"));
        assert!(dbg.contains("Loading…"), "the other day is not known yet");
        assert!(dbg.contains("fn x() {}"), "other code blocks are left alone");
        assert_eq!(b.iter().map(|x| format!("{x:?}")).filter(|s| s.contains("line")).count(), lines_before.len());
        // A plain note's empty block has no day.
        let mut b = blocks("```agenda\n```\n");
        assert!(wanted(&b, None).is_empty());
        fill(&mut b, None, &|_| None, "");
        assert!(format!("{b:?}").contains("no day"));
    }
}
