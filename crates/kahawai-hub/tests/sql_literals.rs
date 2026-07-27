//! Every SQL literal in the crate has to parse against the real schema.
//!
//! Written after two selection queries shipped broken: the HUB-5 rewrite
//! dropped `merged_metadata` from their FROM but left the predicates
//! behind, so the movie/series pass died on a dangling `OR` and the anime
//! pass on `no such column: m.provider`. Neither is reachable from a unit
//! test — one ran only on a full enrichment sweep, and the other's error
//! was swallowed by `unwrap_or_default()`. Both went unnoticed for days.
//!
//! SQLite parses lazily, so a query is only checked when it runs. This is
//! that check, moved to build time: prepare every statement and let the
//! database say whether the columns exist.

use sqlx::{Executor, SqlSafeStr};

/// String literals in a Rust source file, escapes resolved well enough
/// for SQL (`\"` and `\\`; a literal containing `\n` is not SQL we write).
fn string_literals(src: &str) -> Vec<(usize, String)> {
    let (mut out, mut chars, mut line) = (Vec::new(), src.char_indices().peekable(), 1usize);
    while let Some((_, c)) = chars.next() {
        match c {
            '\n' => line += 1,
            // Skip a line comment: `//` inside code, never inside a string
            // (we only get here between literals).
            '/' if chars.peek().map(|(_, c)| *c) == Some('/') => {
                for (_, c) in chars.by_ref() {
                    if c == '\n' {
                        line += 1;
                        break;
                    }
                }
            }
            '"' => {
                let (start, mut lit) = (line, String::new());
                while let Some((_, c)) = chars.next() {
                    match c {
                        '\\' => match chars.next() {
                            Some((_, 'n')) => lit.push('\n'),
                            Some((_, e)) => lit.push(e),
                            None => break,
                        },
                        '"' => break,
                        c => {
                            if c == '\n' {
                                line += 1;
                            }
                            lit.push(c);
                        }
                    }
                }
                out.push((start, lit));
            }
            _ => {}
        }
    }
    out
}

fn is_sql(s: &str) -> bool {
    // A format! template is assembled elsewhere; the view SQL is built
    // that way and is covered by the tests that query it.
    if s.contains('{') {
        return false;
    }
    let upper = s.to_ascii_uppercase();
    let mut words = upper.split_whitespace();
    let (head, next) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    // The keyword alone does not make it a statement: `AFTER {event} ON`
    // clauses in the trigger builder are "INSERT", "DELETE" and
    // "UPDATE OF <cols>", which start with one and parse as none. Demand
    // the shape that always follows the verb.
    match head {
        "INSERT" => next == "INTO" || next == "OR",
        "DELETE" => next == "FROM",
        "UPDATE" => upper.contains(" SET "),
        "SELECT" | "WITH" | "REPLACE" => true,
        _ => false,
    }
}

#[tokio::test]
async fn every_sql_literal_parses_against_the_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    let mut broken = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(&src)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension()? == "rs").then_some(p)
        })
        .collect();
    files.sort();

    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        for (line, lit) in string_literals(&text) {
            if !is_sql(&lit) {
                continue;
            }
            checked += 1;
            // Preparing is enough: it resolves every table and column
            // without touching a row.
            if let Err(e) = db.prepare(sqlx::AssertSqlSafe(lit.clone()).into_sql_str()).await {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                broken.push(format!("{name}:{line}: {e}"));
            }
        }
    }

    assert!(checked > 50, "found only {checked} statements — the scanner stopped working");
    assert!(broken.is_empty(), "{} unparseable statements:\n{}", broken.len(), broken.join("\n"));
}
