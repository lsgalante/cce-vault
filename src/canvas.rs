//! JSON Canvas (<https://jsoncanvas.org>), the format behind Obsidian's
//! `.canvas` files and — from milestone 5 — the cce desktop's own boards.
//!
//! A canvas is held as ordered fields whose values stay as the raw JSON
//! text they were read from (`serde_json::value::RawValue`), never as typed
//! structs or a parsed `Value`. A write must give back every key it did not
//! touch, in its original order and with its original number formatting,
//! including keys a newer Obsidian adds. [`to_string`] reproduces
//! Obsidian's layout byte for byte (tabs, one compact node per line, no
//! trailing newline), so a rename touching a canvas shows up in a diff as
//! the one field it changed.
//!
//! Not `serde_json`'s `preserve_order` feature: features unify across the
//! workspace, and turning it on here would switch every other crate's JSON
//! maps from sorted to insertion order in a `--workspace` build — the
//! compositor's `state.json` included.

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;

use crate::parse::{self, Link, LinkKind, Note};

#[derive(Debug)]
pub struct CanvasError(pub String);

impl std::fmt::Display for CanvasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid canvas: {}", self.0)
    }
}

impl std::error::Error for CanvasError {}

/// One JSON object as ordered (key, raw value) pairs.
#[derive(Debug, Clone)]
pub struct Object(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Object {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Object;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Object, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Box<RawValue>>()? {
                    out.push((k, v));
                }
                Ok(Object(out))
            }
        }
        d.deserialize_map(V)
    }
}

impl Object {
    fn raw(&self, key: &str) -> Option<&RawValue> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| &**v)
    }

    /// A string field's value.
    pub fn str(&self, key: &str) -> Option<String> {
        serde_json::from_str::<String>(self.raw(key)?.get()).ok()
    }

    /// Set a string field in place, or append it when missing.
    pub fn set_str(&mut self, key: &str, value: &str) {
        let raw = serde_json::value::to_raw_value(value).expect("a string always serialises");
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = raw,
            None => self.0.push((key.to_string(), raw)),
        }
    }

    fn write_compact(&self, out: &mut String) {
        out.push('{');
        for (i, (k, v)) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&serde_json::to_string(k).unwrap_or_default());
            out.push(':');
            out.push_str(v.get());
        }
        out.push('}');
    }
}

#[derive(Debug, Clone)]
enum Field {
    /// An array of objects (`nodes`, `edges`): written one per line.
    Objects(Vec<Object>),
    /// Anything else, kept exactly as read.
    Raw(Box<RawValue>),
}

#[derive(Debug, Clone)]
pub struct Canvas {
    fields: Vec<(String, Field)>,
}

pub fn from_str(src: &str) -> Result<Canvas, CanvasError> {
    let top: Object = serde_json::from_str(src).map_err(|e| CanvasError(e.to_string()))?;
    let fields = top
        .0
        .into_iter()
        .map(|(k, v)| {
            let field = match serde_json::from_str::<Vec<Object>>(v.get()) {
                Ok(list) if v.get().trim_start().starts_with('[') => Field::Objects(list),
                _ => Field::Raw(v),
            };
            (k, field)
        })
        .collect();
    Ok(Canvas { fields })
}

/// Obsidian's layout: `JSON.stringify` with a tab indent at the top level
/// and each array element compact on its own line.
pub fn to_string(canvas: &Canvas) -> String {
    let mut out = String::from("{\n");
    let n = canvas.fields.len();
    for (i, (key, field)) in canvas.fields.iter().enumerate() {
        out.push('\t');
        out.push_str(&serde_json::to_string(key).unwrap_or_default());
        out.push(':');
        match field {
            Field::Objects(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (j, item) in items.iter().enumerate() {
                    out.push_str("\t\t");
                    item.write_compact(&mut out);
                    if j + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str("\t]");
            }
            Field::Objects(_) => out.push_str("[]"),
            Field::Raw(v) => out.push_str(v.get()),
        }
        if i + 1 < n {
            out.push(',');
        }
        out.push('\n');
    }
    out.push('}');
    out
}

impl Canvas {
    pub fn nodes(&self) -> impl Iterator<Item = &Object> {
        self.fields.iter().filter(|(k, _)| k == "nodes").flat_map(|(_, f)| match f {
            Field::Objects(list) => list.iter(),
            Field::Raw(_) => Default::default(),
        })
    }

    pub fn nodes_mut(&mut self) -> impl Iterator<Item = &mut Object> {
        self.fields.iter_mut().filter(|(k, _)| k == "nodes").flat_map(|(_, f)| match f {
            Field::Objects(list) => list.iter_mut(),
            Field::Raw(_) => Default::default(),
        })
    }
}

/// A canvas as the index sees it: `file` nodes are embeds of that file,
/// and `text` nodes are small notes whose links, tags and tasks count.
/// Link spans on text-node links are offsets into that node's `text`.
pub fn index(canvas: &Canvas) -> Note {
    let mut note = Note::default();
    for node in canvas.nodes() {
        let id = node.str("id").unwrap_or_default();
        match node.str("type").as_deref() {
            Some("file") => {
                let Some(file) = node.str("file") else { continue };
                note.links.push(Link {
                    kind: LinkKind::CanvasFile,
                    embed: true,
                    target: file,
                    subpath: node
                        .str("subpath")
                        .map(|s| s.trim_start_matches('#').to_string())
                        .filter(|s| !s.is_empty()),
                    display: None,
                    span: 0..0,
                    target_span: 0..0,
                    line: 0,
                    node: Some(id),
                });
            }
            Some("text") => {
                let text = node.str("text").unwrap_or_default();
                let inner = parse::parse(&text);
                note.links.extend(inner.links.into_iter().map(|mut l| {
                    l.node = Some(id.clone());
                    l
                }));
                note.tags.extend(inner.tags);
                note.tasks.extend(inner.tasks);
            }
            _ => {}
        }
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "{\n\t\"nodes\":[\n\
        \t\t{\"id\":\"a1\",\"x\":18,\"y\":-193,\"width\":250,\"height\":60,\"type\":\"text\",\"text\":\"see [[Alpha]] #idea\\n- [ ] do it\"},\n\
        \t\t{\"id\":\"b2\",\"type\":\"file\",\"file\":\"notes/Beta.md\",\"subpath\":\"#Part\",\"x\":0.50,\"y\":1e2,\"width\":400,\"height\":400,\"color\":\"4\"},\n\
        \t\t{\"id\":\"c3\",\"type\":\"link\",\"url\":\"https://example.com\",\"x\":0,\"y\":0,\"width\":1,\"height\":1,\"future\":{\"b\":1,\"a\":[2]}}\n\
        \t],\n\t\"edges\":[\n\
        \t\t{\"id\":\"e1\",\"fromNode\":\"a1\",\"fromSide\":\"right\",\"toNode\":\"b2\",\"toSide\":\"left\"}\n\
        \t],\n\t\"zeta\":{\"keep\": \"as written\"}\n}";

    #[test]
    fn round_trip_is_byte_exact() {
        let canvas = from_str(SAMPLE).unwrap();
        assert_eq!(to_string(&canvas), SAMPLE);
        let empty = "{\n\t\"nodes\":[],\n\t\"edges\":[]\n}";
        assert_eq!(to_string(&from_str(empty).unwrap()), empty);
        assert!(from_str("[1]").is_err());
    }

    /// Every `.canvas` under `$CCE_CANVAS_DIR` must survive a read and a
    /// write unchanged: `CCE_CANVAS_DIR=~/vault cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn real_canvases_round_trip() {
        let dir = std::env::var("CCE_CANVAS_DIR").expect("set CCE_CANVAS_DIR");
        let mut checked = 0;
        for entry in walkdir::WalkDir::new(dir).into_iter().flatten() {
            if entry.path().extension().is_some_and(|e| e == "canvas") {
                let text = std::fs::read_to_string(entry.path()).unwrap();
                assert_eq!(to_string(&from_str(&text).unwrap()), text, "{}", entry.path().display());
                checked += 1;
            }
        }
        assert!(checked > 0, "no canvases found");
    }

    #[test]
    fn index_file_and_text_nodes() {
        let note = index(&from_str(SAMPLE).unwrap());
        let got: Vec<_> = note
            .links
            .iter()
            .map(|l| (l.target.as_str(), l.kind, l.node.as_deref(), l.subpath.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("Alpha", LinkKind::Wiki, Some("a1"), None),
                ("notes/Beta.md", LinkKind::CanvasFile, Some("b2"), Some("Part")),
            ]
        );
        assert_eq!(note.tags[0].name, "idea");
        assert_eq!(note.tasks[0].text, "do it");
    }

    #[test]
    fn edit_changes_only_that_field() {
        let mut canvas = from_str(SAMPLE).unwrap();
        for node in canvas.nodes_mut() {
            if node.str("file").as_deref() == Some("notes/Beta.md") {
                node.set_str("file", "notes/Gamma \"quoted\".md");
            }
        }
        let expected = SAMPLE.replace("notes/Beta.md", "notes/Gamma \\\"quoted\\\".md");
        assert_eq!(to_string(&canvas), expected);
    }
}
