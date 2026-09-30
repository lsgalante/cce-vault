//! `cce-vault`: the vault index from a shell — for scripts, agents and
//! shadow tests, and the quickest way to check what the apps will see.

use std::path::PathBuf;
use std::process::ExitCode;

use cce_vault::{config, search, FileKind, Index};
use chrono::{Duration, Local, NaiveDate};
use serde_json::{json, Value};

const HELP: &str = "\
cce-vault — query and edit the notes vault

usage: cce-vault [--vault DIR] [--json] [--cache] <command> [args]

  path                        the vault root
  stats                       files, links, tags and tasks; index timing
  find <query>                fuzzy-match note names, aliases and paths
  search <query>              full text; every word must match, \"quote phrases\"
  resolve <link> [--from N]   the file link text reaches (from note N)
  links <note>                outgoing links and where each resolves
  backlinks <note>            links pointing at the note
  mentions <note>             unlinked mentions of the note's name or aliases
  unresolved                  links that reach no file
  tags [tag]                  tag counts, or the notes carrying a tag
  tasks [--done|--all] [note] open tasks (vault-wide, or one note)
  properties <note>           frontmatter as JSON
  daily [date] [--create]     the daily note's path; date is YYYY-MM-DD,
                              today, yesterday, tomorrow, or +N / -N days
  rename <note> <to> [--dry-run]
                              move a file and rewrite every link to it; a
                              bare new name stays in the same folder
  watch                       apply and print changes as they happen

The vault is --vault, else $CCE_VAULT, else `vault { path \"…\" }` in
~/.config/cce/config.kdl. Lines are printed 1-based. --cache reuses (and
writes) the parse cache under ~/.cache/cce/vault; off by default, since a
parallel parse of a warm vault is faster than loading it.
";

struct Opts {
    vault: Option<PathBuf>,
    json: bool,
    cache: bool,
    args: Vec<String>,
}

fn parse_opts() -> Result<Opts, String> {
    let mut opts = Opts { vault: None, json: false, cache: false, args: Vec::new() };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--vault" => opts.vault = Some(it.next().ok_or("--vault needs a directory")?.into()),
            "--json" => opts.json = true,
            "--cache" => opts.cache = true,
            "-h" | "--help" | "help" => opts.args = vec!["help".into()],
            _ => opts.args.push(a),
        }
    }
    Ok(opts)
}

/// Pull `--flag` out of the positional args.
fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}

fn take_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

fn main() -> ExitCode {
    env_logger_lite();
    let opts = match parse_opts() {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    if opts.args.is_empty() || opts.args[0] == "help" {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let root = match config::vault_root(opts.vault.as_deref()) {
        Ok(r) => r,
        Err(e) => return fail(&e.to_string()),
    };
    let mut index = match Index::open(&root, opts.cache) {
        Ok(i) => i,
        Err(e) => return fail(&format!("{}: {e}", root.display())),
    };
    let result = run(&mut index, &opts);
    if opts.cache {
        if let Err(e) = index.save_cache() {
            log_warn(&format!("could not write the index cache: {e}"));
        }
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("cce-vault: {msg}");
    ExitCode::FAILURE
}

fn log_warn(msg: &str) {
    eprintln!("cce-vault: warning: {msg}");
}

/// `RUST_LOG=debug` shows the crate's log lines on stderr without pulling
/// in a logging stack for a CLI.
fn env_logger_lite() {
    struct Stderr(log::LevelFilter);
    impl log::Log for Stderr {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= self.0
        }
        fn log(&self, r: &log::Record) {
            if self.enabled(r.metadata()) {
                eprintln!("[{}] {}", r.level(), r.args());
            }
        }
        fn flush(&self) {}
    }
    let level = match std::env::var("RUST_LOG").unwrap_or_default().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        _ => log::LevelFilter::Warn,
    };
    let _ = log::set_logger(Box::leak(Box::new(Stderr(level)))).map(|_| log::set_max_level(level));
}

/// A note named on the command line, or an error that suggests names.
fn note_arg(index: &Index, arg: Option<&String>) -> Result<String, String> {
    let q = arg.ok_or("which note?")?;
    if let Some(p) = index.lookup(q) {
        return Ok(p);
    }
    let near: Vec<String> = index.find(q, 3).into_iter().map(|m| m.path).collect();
    if near.is_empty() {
        Err(format!("no note matches {q:?}"))
    } else {
        Err(format!("no note matches {q:?}; did you mean: {}", near.join(", ")))
    }
}

fn out_json(v: Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

fn run(index: &mut Index, opts: &Opts) -> Result<(), String> {
    let mut args = opts.args.clone();
    let cmd = args.remove(0);
    let json = opts.json;
    match cmd.as_str() {
        "path" => println!("{}", index.root().display()),

        "stats" => {
            let count = |k| index.files().values().filter(|e| e.kind == k).count();
            let links: usize = index.documents().map(|(_, n)| n.links.len()).sum();
            let unresolved: usize = index.unresolved().values().map(Vec::len).sum();
            let tasks: Vec<_> = index.tasks().collect();
            let open = tasks.iter().filter(|(_, t)| t.is_open()).count();
            let rows: Vec<(&str, Value)> = vec![
                ("root", json!(index.root())),
                ("notes", json!(count(FileKind::Note))),
                ("canvases", json!(count(FileKind::Canvas))),
                ("attachments", json!(count(FileKind::Attachment))),
                ("links", json!(links)),
                ("unresolved_links", json!(unresolved)),
                ("tags", json!(index.tags().len())),
                ("tasks", json!(tasks.len())),
                ("open_tasks", json!(open)),
                ("index.parsed", json!(index.stats.parsed)),
                ("index.reused", json!(index.stats.reused)),
                ("index.millis", json!(index.stats.millis)),
            ];
            if json {
                out_json(Value::Object(rows.into_iter().map(|(k, v)| (k.to_string(), v)).collect()));
            } else {
                for (k, v) in rows {
                    match v {
                        Value::String(s) => println!("{k:18} {s}"),
                        other => println!("{k:18} {other}"),
                    }
                }
            }
        }

        "find" => {
            let q = args.join(" ");
            let hits = index.find(&q, 20);
            if json {
                out_json(json!(hits));
            } else {
                for h in hits {
                    if h.matched == cce_vault::index::stem(&h.path) || h.matched == h.path {
                        println!("{}", h.path);
                    } else {
                        println!("{}  (as {:?})", h.path, h.matched);
                    }
                }
            }
        }

        "search" => {
            let q = args.join(" ");
            let hits = index.search(&q, 50);
            print_hits(&hits, json);
        }

        "resolve" => {
            let from = match take_value(&mut args, "--from") {
                Some(f) => Some(note_arg(index, Some(&f))?),
                None => None,
            };
            let text = args.join(" ");
            let hit = index.resolve_text(from.as_deref(), &text);
            if json {
                out_json(json!({ "link": text, "from": from, "path": hit }));
            } else {
                println!("{}", hit.ok_or_else(|| format!("{text:?} reaches no file"))?);
            }
        }

        "links" => {
            let path = note_arg(index, args.first())?;
            let out = index.outgoing(&path);
            if json {
                out_json(json!(out
                    .iter()
                    .map(|(l, t)| json!({ "line": l.line + 1, "link": l, "resolved": t }))
                    .collect::<Vec<_>>()));
            } else {
                for (l, t) in out {
                    let sub = l.subpath.as_ref().map(|s| format!("#{s}")).unwrap_or_default();
                    let to = t.map(String::from).unwrap_or_else(|| "(unresolved)".into());
                    println!("{path}:{}: {}{sub} -> {to}", l.line + 1, l.target);
                }
            }
        }

        "backlinks" => {
            let path = note_arg(index, args.first())?;
            let bl = index.backlinks(&path);
            if json {
                out_json(json!(bl
                    .iter()
                    .map(|b| json!({ "source": b.source, "line": b.link.line + 1, "link": b.link }))
                    .collect::<Vec<_>>()));
            } else {
                for b in bl {
                    let sub = b.link.subpath.as_ref().map(|s| format!("#{s}")).unwrap_or_default();
                    let node = b.link.node.as_ref().map(|n| format!(" (node {n})")).unwrap_or_default();
                    println!("{}:{}: {}{sub}{node}", b.source, b.link.line + 1, b.link.target);
                }
            }
        }

        "mentions" => {
            let path = note_arg(index, args.first())?;
            print_hits(&index.unlinked_mentions(&path), json);
        }

        "unresolved" => {
            let un = index.unresolved();
            if json {
                out_json(json!(un
                    .iter()
                    .map(|(t, bl)| (
                        t.clone(),
                        json!(bl.iter().map(|b| json!({ "source": b.source, "line": b.link.line + 1 })).collect::<Vec<_>>())
                    ))
                    .collect::<serde_json::Map<_, _>>()));
            } else {
                for (target, bl) in un {
                    let from: Vec<String> = bl.iter().map(|b| format!("{}:{}", b.source, b.link.line + 1)).collect();
                    println!("{target}  <- {}", from.join(", "));
                }
            }
        }

        "tags" => match args.first() {
            Some(tag) => {
                let notes = index.tagged(tag);
                if json {
                    out_json(json!(notes));
                } else {
                    notes.iter().for_each(|n| println!("{n}"));
                }
            }
            None => {
                let tags = index.tags();
                if json {
                    out_json(json!(tags.iter().map(|(t, n)| (t.clone(), json!(n))).collect::<serde_json::Map<_, _>>()));
                } else {
                    for (t, n) in tags {
                        println!("{n:5} #{t}");
                    }
                }
            }
        },

        "tasks" => {
            let done = take_flag(&mut args, "--done");
            let all = take_flag(&mut args, "--all");
            let only = match args.first() {
                Some(a) => Some(note_arg(index, Some(a))?),
                None => None,
            };
            let tasks: Vec<_> = index
                .tasks()
                .filter(|(p, _)| only.as_deref().is_none_or(|o| o == *p))
                .filter(|(_, t)| all || (t.is_open() != done))
                .collect();
            if json {
                out_json(json!(tasks
                    .iter()
                    .map(|(p, t)| json!({ "path": p, "line": t.line + 1, "status": t.status.to_string(), "text": t.text }))
                    .collect::<Vec<_>>()));
            } else {
                for (p, t) in tasks {
                    println!("{p}:{}: [{}] {}", t.line + 1, t.status, t.text);
                }
            }
        }

        "properties" => {
            let path = note_arg(index, args.first())?;
            let props = index.note(&path).map(|n| n.properties.clone()).unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&props).unwrap_or_default());
        }

        "daily" => {
            let create = take_flag(&mut args, "--create");
            let date = parse_date(args.first().map(String::as_str).unwrap_or("today"))?;
            let (path, created) = index.daily(date, create).map_err(|e| e.to_string())?;
            if json {
                let exists = index.entry(&path).is_some();
                out_json(json!({ "path": path, "date": date.to_string(), "exists": exists, "created": created }));
            } else {
                println!("{path}");
            }
        }

        "rename" => {
            let dry = take_flag(&mut args, "--dry-run");
            let from = note_arg(index, args.first())?;
            let to_arg = args.get(1).ok_or("rename to what?")?;
            let mut to = if to_arg.contains('/') {
                to_arg.trim_start_matches('/').to_string()
            } else {
                match cce_vault::index::parent(&from) {
                    "" => to_arg.clone(),
                    dir => format!("{dir}/{to_arg}"),
                }
            };
            // `rename Old New` for a note means New.md.
            if FileKind::of(&from) == FileKind::Note && FileKind::of(&to) != FileKind::Note {
                to.push_str(".md");
            }
            let plan = if dry { index.plan_rename(&from, &to) } else { index.rename(&from, &to) }
                .map_err(|e| e.to_string())?;
            if json {
                out_json(json!({ "dry_run": dry, "plan": plan }));
            } else {
                println!("{}{} -> {}", if dry { "would move " } else { "moved " }, plan.from, plan.to);
                for e in &plan.edits {
                    println!("  {}:{}: {} -> {}", e.path, e.line + 1, e.old, e.new);
                }
                println!("{} link{} {}", plan.edits.len(), if plan.edits.len() == 1 { "" } else { "s" },
                    if dry { "would change" } else { "rewritten" });
            }
        }

        "watch" => {
            let (tx, rx) = std::sync::mpsc::channel();
            let _w = cce_vault::VaultWatcher::spawn(index.root(), move |batch| {
                let _ = tx.send(batch);
            })
            .map_err(|e| e.to_string())?;
            eprintln!("watching {} (ctrl-c to stop)", index.root().display());
            for batch in rx {
                let ch = index.apply_changes(&batch);
                if ch.is_empty() {
                    continue;
                }
                if json {
                    println!("{}", json!(ch));
                } else {
                    ch.updated.iter().for_each(|p| println!("updated {p}"));
                    ch.removed.iter().for_each(|p| println!("removed {p}"));
                }
                if opts.cache {
                    let _ = index.save_cache();
                }
            }
        }

        other => return Err(format!("unknown command {other:?} (try --help)")),
    }
    Ok(())
}

fn print_hits(hits: &[search::FileHits], json: bool) {
    if json {
        out_json(json!(hits));
        return;
    }
    for h in hits {
        for l in &h.lines {
            println!("{}:{}: {}", h.path, l.line + 1, l.text);
        }
        if h.total > h.lines.len() {
            println!("{}: … {} more", h.path, h.total - h.lines.len());
        }
    }
}

fn parse_date(s: &str) -> Result<NaiveDate, String> {
    let today = Local::now().date_naive();
    match s {
        "today" => Ok(today),
        "yesterday" => Ok(today - Duration::days(1)),
        "tomorrow" => Ok(today + Duration::days(1)),
        _ if s.starts_with('+') || s.starts_with('-') => s
            .parse::<i64>()
            .map(|n| today + Duration::days(n))
            .map_err(|_| format!("not a day offset: {s}")),
        _ => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("not a date (YYYY-MM-DD): {s}")),
    }
}
