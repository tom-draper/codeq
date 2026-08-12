//! The `codeq.result.v2` output envelope.
//!
//! Designed for the actual consumer: an AI agent paying tokens per byte. So:
//!
//! - **Compact by default.** Pretty-printing is opt-in (`--meta`/`--pretty`);
//!   the default is single-line JSON.
//! - **Meta only when it carries signal.** Because codeq refreshes on every
//!   read, answers are almost always fresh — so `repo`/`index` blocks are
//!   omitted unless something is noteworthy (files reparsed, staleness, a
//!   partial failure) or the caller asks with `--meta`.
//! - **Projectable.** `--fields` drops columns the agent did not ask for.
//!
//! # ROW_KEY_RULE
//!
//! Every row, from every command, keys the same fact the same way. One rule,
//! because `--fields`/`--json` consumers move between commands and an agent that
//! learns a key on one must be able to use it on the next:
//!
//! - **`name`** — the definition this row *is*.
//! - **`symbol`** — the definition this row sits *inside* (a `search` or
//!   `references` match). Never the row's own identity.
//! - **`line`, `col`, `byte`** — the row's primary position.
//! - **`end_line`, `end_col`, `end_byte`** — its extent, where one is known.
//!
//! The rule covers keys *within* a row. Which container holds the rows still
//! varies by command, and legitimately so — `search -l` returns `files` where
//! plain `search` returns `matches`. Under `--json` the envelope therefore
//! carries **`rows`**, the dotted path to them (`"data"`, `"data.matches"`), so a
//! consumer never has to know the shape in advance. See `rows_pointer`.
//!
//! Rust fields may keep the `start_` prefix that mirrors the facts table; the
//! emitted key is what this rule governs, so those carry `#[serde(rename)]`.
//! v1 leaked the internal spelling: `definitions`/`symbols`/`imports`/`outline`
//! emitted `start_line` while eight other commands emitted `line`, so
//! `--fields line` worked on some and silently matched nothing on the rest, and
//! `render_row` carried an `or_else(start_line)` fallback to hide it in text.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

pub const RESULT_SCHEMA: &str = "codeq.result.v2";

/// Output rendering options, threaded from the CLI.
#[derive(Clone, Copy, Default)]
pub struct Render<'a> {
    /// Emit JSON. Default is compact grep-style text. `meta`/`pretty` imply this.
    pub json: bool,
    /// Force the full `repo`/`index` envelope even when nothing is noteworthy.
    pub meta: bool,
    /// Pretty-print (implies including meta for human reading).
    pub pretty: bool,
    /// If set, keep only these keys on each object in `data`.
    pub fields: Option<&'a [String]>,
    /// Group text rows under a `path (count)` header rather than prefixing every
    /// row with its path. Text only — JSON is already structured.
    pub group: bool,
    /// Keep at most this many rows per file. Unlike `group`, this is *lossy*, so
    /// it changes `data` itself rather than only the text rendering: text and
    /// JSON must never disagree about which rows the answer contains.
    pub max_per_file: Option<usize>,
}

pub struct Envelope<T: Serialize> {
    repo: RepoMeta,
    index: IndexMeta,
    data: T,
    /// Why an empty result is empty. Carried in the envelope rather than only on
    /// stderr because the MCP server captures stdout alone, and the agent behind
    /// it is exactly who needs to know the difference between "none" and "you
    /// asked the wrong question".
    note: Option<String>,
}

impl<T: Serialize> Envelope<T> {
    pub fn new(repo: RepoMeta, index: IndexMeta, data: T) -> Self {
        Envelope {
            repo,
            index,
            data,
            note: None,
        }
    }

    pub fn note(mut self, note: Option<String>) -> Self {
        self.note = note;
        self
    }

    /// Whether the meta blocks carry signal worth the tokens.
    fn noteworthy(&self) -> bool {
        self.index.reindexed_files > 0
            || !self.index.fresh
            || self.index.partial_failures > 0
    }

    pub fn print(&self, r: Render) -> anyhow::Result<()> {
        let mut data = serde_json::to_value(&self.data)?;

        // Lossy, so it happens here on `data` — the same place `--fields`
        // projects — and therefore applies to JSON and text alike. `truncated`
        // maps each capped file to its true count, which is what stops this
        // from being a silent drop: the answer always says how much it withheld.
        //
        // It must run *before* `project`, which can drop the `path` column the
        // cap groups by: `--fields symbol,line --max-per-file 2` would otherwise
        // find no paths and silently keep every row — two token-reducing flags
        // where asking for both gives you less reduction than asking for one.
        let truncated = match r.max_per_file {
            Some(n) if n > 0 => cap_per_file(&mut data, n),
            _ => BTreeMap::new(),
        };

        // `(requested-but-absent, actually-available)`; empty when no rows came
        // back, since there is then nothing to judge a field name against.
        let (unmatched, available) = match r.fields {
            Some(fields) => project(&mut data, fields),
            None => (Vec::new(), Vec::new()),
        };
        let note_fields = !unmatched.is_empty() && !available.is_empty();

        // JSON is opt-in (`--json`); `--meta`/`--pretty` are JSON concepts so they
        // imply it. The default is compact grep-style text — far fewer tokens.
        if r.json || r.meta || r.pretty {
            let include_meta = r.meta || r.pretty || self.noteworthy();
            let mut out = serde_json::Map::new();
            out.insert("schema".into(), Value::from(RESULT_SCHEMA));
            if let Some(n) = &self.note {
                out.insert("note".into(), Value::from(n.as_str()));
            }
            if include_meta {
                out.insert("repo".into(), serde_json::to_value(&self.repo)?);
                out.insert("index".into(), serde_json::to_value(&self.index)?);
            }
            if let Some(p) = rows_pointer(&data) {
                out.insert("rows".into(), Value::from(p));
            }
            out.insert("data".into(), data);
            if note_fields || !truncated.is_empty() {
                let mut notes = serde_json::Map::new();
                if note_fields {
                    notes.insert("unmatched_fields".into(), serde_json::to_value(&unmatched)?);
                    notes.insert("available_fields".into(), serde_json::to_value(&available)?);
                }
                if !truncated.is_empty() {
                    // Per file, the count *before* capping — so a consumer can
                    // tell "3 hits" from "3 of 240 hits" and go back for the rest.
                    notes.insert("truncated_per_file".into(), serde_json::to_value(&truncated)?);
                }
                out.insert("notes".into(), Value::Object(notes));
            }
            let value = Value::Object(out);
            if r.pretty {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{}", serde_json::to_string(&value)?);
            }
            return Ok(());
        }

        // Text mode: keep stdout pure data; surface noteworthy meta on stderr so
        // it doesn't pollute a parseable/piped result.
        if self.noteworthy() {
            eprintln!(
                "# codeq: reindexed {} file(s){}{}",
                self.index.reindexed_files,
                if self.index.partial_failures > 0 {
                    format!(", {} partial failure(s)", self.index.partial_failures)
                } else {
                    String::new()
                },
                if self.index.fresh { "" } else { ", index stale" },
            );
        }
        if note_fields {
            eprintln!(
                "# codeq: --fields {} matched no row; available: {}",
                unmatched.join(","),
                available.join(",")
            );
        }
        let mut s = String::new();
        render_value(&data, r.group, &truncated, &mut s);
        print!("{s}");
        if !truncated.is_empty() {
            let dropped: usize = truncated.values().sum();
            eprintln!(
                "# codeq: --max-per-file kept {} of {dropped} rows across {} file(s); rerun without it for the rest",
                r.max_per_file.unwrap_or(0) * truncated.len(),
                truncated.len(),
            );
        }
        // `data.truncated` says the result stopped at `--limit`. JSON has carried
        // it all along; text rendered only the rows, so a capped-at-100 answer
        // read exactly like a complete one — the one thing every other truncation
        // in codeq is careful to never do.
        if hit_limit(&data) {
            eprintln!(
                "# codeq: result stopped at --limit; rerun with a higher --limit (or 0) for the rest"
            );
        }
        // Text mode keeps stdout pure data, so the note goes to stderr where it
        // is visible to a person and invisible to a pipe.
        if let Some(n) = &self.note {
            eprintln!("# codeq: {n}");
        }
        Ok(())
    }
}

/// Render a bare `data` value as codeq's compact text output, for commands that
/// don't flow through [`Envelope`] (e.g. `capabilities`, which is session-less).
pub fn render_data_text(data: &Value) -> String {
    let mut s = String::new();
    render_value(data, false, &BTreeMap::new(), &mut s);
    s
}

/// Render a `data` payload as compact, deterministic, grep-style text.
///
/// - array of rows → one `path:line:col: payload` line each;
/// - object with a single primary array (`matches`, `references`, …) → its rows;
/// - object with several arrays (`about`, `map`) → each under a `# key` header;
/// - object of scalars (`stats`) → `key: value` lines.
/// Did this result stop at `--limit`? Commands that truncate say so with a
/// `truncated: true` sibling of their row array; the text renderer drops scalars
/// next to a single row list, so it is read here instead and reported on stderr.
fn hit_limit(data: &Value) -> bool {
    data.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Keep at most `n` rows per `path`, returning `path -> count before capping`
/// for the files that actually lost rows.
///
/// Rows with no `path` are never dropped: the cap is a per-file idea, and a
/// pathless row (a bare identifier, a cycle) belongs to no file.
/// Where this result's rows live, as a dotted path from the envelope root:
/// `"data"` when `data` is itself the array, `"data.matches"` when it wraps one.
/// `None` when there is no single answer — `about` and `map` carry several
/// arrays that are genuinely different lists, and `capabilities` carries none.
///
/// `ROW_KEY_RULE` made every row *key* mean the same thing on every command, but
/// the *container* still varies, and it has to: `search -l` returns `files` where
/// plain `search` returns `matches`, and those are different facts, not different
/// spellings. So rather than force a shape, the envelope says which one it used —
/// a consumer reads `rows` instead of knowing thirty commands in advance. Derived
/// from the value, so a new command is described without being registered.
fn rows_pointer(data: &Value) -> Option<String> {
    match data {
        Value::Array(_) => Some("data".to_string()),
        Value::Object(map) => {
            let mut arrays = map.iter().filter(|(_, v)| v.is_array());
            let (key, _) = arrays.next()?;
            // More than one list: no single thing to point at, and guessing
            // would be worse than saying nothing.
            arrays.next().is_none().then(|| format!("data.{key}"))
        }
        _ => None,
    }
}

fn cap_per_file(data: &mut Value, n: usize) -> BTreeMap<String, usize> {
    let mut truncated = BTreeMap::new();
    match data {
        Value::Array(rows) => cap_rows(rows, n, &mut truncated),
        Value::Object(map) => {
            for v in map.values_mut() {
                if let Value::Array(rows) = v {
                    cap_rows(rows, n, &mut truncated);
                }
            }
        }
        _ => {}
    }
    truncated
}

fn cap_rows(rows: &mut Vec<Value>, n: usize, truncated: &mut BTreeMap<String, usize>) {
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    for r in rows.iter() {
        if let Some(p) = r.get("path").and_then(|v| v.as_str()) {
            *totals.entry(p.to_string()).or_insert(0) += 1;
        }
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    rows.retain(|r| match r.get("path").and_then(|v| v.as_str()) {
        Some(p) => {
            let c = seen.entry(p.to_string()).or_insert(0);
            *c += 1;
            *c <= n
        }
        None => true,
    });
    for (p, total) in totals {
        if total > n {
            truncated.insert(p, total);
        }
    }
}

fn render_value(data: &Value, group: bool, truncated: &BTreeMap<String, usize>, out: &mut String) {
    match data {
        Value::Array(rows) => render_rows(rows, group, truncated, out),
        Value::Object(map) => {
            let arrays = map.values().filter(|v| v.is_array()).count();
            if arrays == 0 {
                for (k, v) in map {
                    out.push_str(&format!("{k}: {}\n", inline(v)));
                }
            } else if arrays == 1 && map.len() <= 2 {
                // A single result list (+ maybe a scalar like `truncated`): just rows.
                for v in map.values() {
                    if let Value::Array(rows) = v {
                        render_rows(rows, group, truncated, out);
                    }
                }
            } else {
                for (k, v) in map {
                    match v {
                        Value::Array(rows) => {
                            out.push_str(&format!("# {k}\n"));
                            render_rows(rows, group, truncated, out);
                        }
                        _ => out.push_str(&format!("{k}: {}\n", inline(v))),
                    }
                }
            }
        }
        other => {
            out.push_str(&inline(other));
            out.push('\n');
        }
    }
}

/// Render one list of rows, normalizing the `depth` column so indentation is
/// relative to the shallowest row. `outline` numbers top-level definitions 0
/// while `callers`/`callees`/`deps` number direct hits 1; without this every
/// row of a `--depth` walk would carry a leading indent that means nothing.
fn render_rows(
    rows: &[Value],
    group: bool,
    truncated: &BTreeMap<String, usize>,
    out: &mut String,
) {
    let base = rows
        .iter()
        .filter_map(|r| r.get("depth").and_then(|v| v.as_i64()))
        .min()
        .unwrap_or(0);
    if !group && truncated.is_empty() {
        for row in rows {
            render_row(row, base, false, out);
        }
        return;
    }
    // Group *consecutive* runs of the same path rather than sorting into buckets:
    // row order is part of the answer (a caller walk is ordered by depth, search
    // by position), so reordering to make groups tidier would corrupt it. Rows
    // already arrive path-ordered from SQL, so runs are the natural grouping; if
    // they aren't, a path simply heads more than one block, which is honest.
    let mut i = 0;
    while i < rows.len() {
        let path = rows[i].get("path").and_then(|v| v.as_str());
        let Some(p) = path else {
            // No path to hoist (a bare identifier row, a cycle, `--fields` having
            // dropped the column): nothing to group by, so render it flat.
            render_row(&rows[i], base, false, out);
            i += 1;
            continue;
        };
        let mut j = i;
        while j < rows.len() && rows[j].get("path").and_then(|v| v.as_str()) == Some(p) {
            j += 1;
        }
        let shown = j - i;
        let total = truncated.get(p).copied();
        if group {
            // The count is why grouping stays lossless: the agent gets the group
            // size without counting lines. When the file was capped the header
            // says `3 of 240`, so a truncated answer can never read as a
            // complete one.
            match total {
                Some(t) => out.push_str(&format!("{p} ({shown} of {t})\n")),
                None => out.push_str(&format!("{p} ({shown})\n")),
            }
        }
        for row in &rows[i..j] {
            render_row(row, base, group, out);
        }
        // Flat rows carry no header, so a capped file needs its own marker —
        // led by the path, so the line stays self-describing like every other.
        if !group {
            if let Some(t) = total {
                out.push_str(&format!("{p}: +{} more (of {t})\n", t - shown));
            }
        }
        i = j;
    }
}

/// One result row → `path:line:col: payload`, indented by `depth` for outlines.
fn render_row(row: &Value, base_depth: i64, in_group: bool, out: &mut String) {
    let o = match row {
        Value::Object(m) => m,
        other => {
            out.push_str(&inline(other));
            out.push('\n');
            return;
        }
    };
    let str_of = |k: &str| o.get(k).and_then(|v| v.as_str());
    let int_of = |k: &str| o.get(k).and_then(|v| v.as_i64());

    // Under a group header the path is already stated, so the row carries only
    // `line:col` — but it keeps the column, which the pathless fallback below
    // drops. Losing `col` here would make grouped output strictly less precise
    // than flat, and the two must carry the same facts.
    let mut loc = String::new();
    if let Some(p) = str_of("path") {
        // One key, per ROW_KEY_RULE. This used to fall back to `start_line`,
        // which let the schema split survive unnoticed in text mode.
        let line = int_of("line");
        if !in_group {
            loc.push_str(p);
        }
        if let Some(l) = line {
            if in_group {
                loc.push_str(&format!("{l}"));
            } else {
                loc.push_str(&format!(":{l}"));
            }
            if let Some(c) = int_of("col") {
                loc.push_str(&format!(":{c}"));
            }
        }
        // A path-only row (`files`, `tree`) has nothing left once the group
        // header takes the path, so the separator would lead the line: `: rust
        // size_bytes=…`. Emit the payload bare instead.
        if !in_group || line.is_some() {
            loc.push_str(": ");
        }
    } else if let Some(l) = int_of("line") {
        loc.push_str(&format!("{l}: "));
    }

    let pad = "  ".repeat((int_of("depth").unwrap_or(0) - base_depth).max(0) as usize);

    // A multi-line body (context chunks) prints verbatim under a locator header
    // (drop the trailing ": " so the header reads cleanly above the block). For
    // `context` chunks the `reason` (seed vs referenced/changed) is the point of
    // --expand, so surface it in the header.
    if let Some(t) = str_of("text") {
        if t.contains('\n') {
            let reason = str_of("reason").map(|r| format!(" ({r})")).unwrap_or_default();
            out.push_str(&format!("{pad}{}{reason}\n{t}\n", loc.trim_end_matches([':', ' '])));
            return;
        }
        // codeq's differentiator over grep: annotate the match with the symbol
        // it sits inside, so an agent needn't a follow-up `at` to place it.
        let enclosing = str_of("symbol")
            .map(|s| format!("  [{s}]"))
            .unwrap_or_default();
        // The receiver a call is made on. Shown rather than used to filter: a
        // receiver can be a value (`db.query()`) or a module path
        // (`helpers::stem()`), and telling those apart needs name resolution
        // codeq does not do. Surfacing it lets the reader judge.
        let on = str_of("receiver")
            .map(|r| format!("  on={r}"))
            .unwrap_or_default();
        let match_line = format!("{pad}{loc}{}{enclosing}{on}\n", t.trim());

        // -A/-B/-C context lines: render grep-style (`path-line- content`) with
        // computed line numbers, so the flag isn't a silent no-op in text mode.
        let before = o.get("before").and_then(|v| v.as_array());
        let after = o.get("after").and_then(|v| v.as_array());
        if let (Some(p), Some(ln)) = (str_of("path"), int_of("line")) {
            if before.is_some() || after.is_some() {
                // Grouped context lines drop the path for the same reason the
                // match line does — the header already carries it — and with it
                // the separating `-`, so the row reads `196- code` rather than
                // leading with a stray dash.
                let lead = if in_group { String::new() } else { format!("{p}-") };
                if let Some(b) = before {
                    let start = ln - b.len() as i64;
                    for (i, cl) in b.iter().filter_map(|v| v.as_str()).enumerate() {
                        out.push_str(&format!("{pad}{lead}{}- {}\n", start + i as i64, cl.trim_end()));
                    }
                }
                out.push_str(&match_line);
                if let Some(a) = after {
                    for (i, cl) in a.iter().filter_map(|v| v.as_str()).enumerate() {
                        out.push_str(&format!("{pad}{lead}{}- {}\n", ln + 1 + i as i64, cl.trim_end()));
                    }
                }
                return;
            }
        }
        out.push_str(&match_line);
        return;
    }
    // A definition's own description of itself, which is the most useful thing
    // after the signature. Docs are stored already collapsed to one line and
    // capped, so appending keeps one row on one line. Only the commands that ask
    // for docs put the field on a row, so this costs nothing elsewhere.
    let doc = str_of("doc")
        .map(|d| format!("  // {}", d.trim()))
        .unwrap_or_default();

    if let Some(sig) = str_of("signature") {
        // A member's signature (`pub retries: u32`) says nothing about which type
        // it belongs to, and for a name like `id` or `name` that is the only part
        // that identifies it. Bracketed like `search`'s enclosing symbol, before
        // the doc comment so the row reads signature -> owner -> prose.
        match str_of("owner") {
            Some(owner) => out.push_str(&format!("{pad}{loc}{sig}  [{owner}]{doc}\n")),
            None => out.push_str(&format!("{pad}{loc}{sig}{doc}\n")),
        }
        return;
    }
    // An import cycle: the ordered file list, closed back to its first member so
    // the loop is visible (`a -> b -> a`).
    if let Some(cycle) = o.get("cycle").and_then(|v| v.as_array()) {
        let files: Vec<&str> = cycle.iter().filter_map(|v| v.as_str()).collect();
        if !files.is_empty() {
            out.push_str(&format!("{pad}{} -> {}\n", files.join(" -> "), files[0]));
            return;
        }
    }
    // Bare identifiers (symbols, callers, imports, map groupings), then any other
    // scalar fields as `key=value` — so metric rows like map's `definitions`/
    // `files`/`importers` counts render instead of vanishing.
    let mut parts: Vec<String> = Vec::new();
    for key in ["kind", "name", "module", "symbol", "dir"] {
        if let Some(v) = str_of(key) {
            parts.push(inline(&Value::from(v)));
        }
    }
    if parts.is_empty() {
        if let Some(l) = str_of("language") {
            parts.push(l.to_string()); // language identifies a row only when nothing else does
        }
    }
    // Keys the locator/payload above already rendered, so the generic `k=v` tail
    // below doesn't print them twice. Position keys follow ROW_KEY_RULE; the
    // `start_*` spellings are gone from the wire, so they are gone from here too.
    const CONSUMED: &[&str] = &[
        "path", "line", "col", "byte", "end_line", "end_col", "end_byte", "depth",
        "text", "signature", "doc", "source", "receiver", "id", "confidence",
        "language", "kind", "name", "module", "symbol", "dir",
    ];
    for (k, v) in o {
        if CONSUMED.contains(&k.as_str()) {
            continue;
        }
        if v.is_number() || v.is_boolean() || v.is_string() {
            parts.push(format!("{k}={}", inline(v)));
        }
    }
    // A row with no payload (e.g. a caller sitting at file scope, so no enclosing
    // symbol) is just its location: drop the dangling ": " rather than emit a
    // separator with nothing after it.
    let payload = parts.join(" ");
    if payload.is_empty() && doc.is_empty() {
        out.push_str(&format!("{pad}{}\n", loc.trim_end_matches([':', ' '])));
    } else {
        out.push_str(&format!("{pad}{loc}{payload}{doc}\n"));
    }
}

/// Render a scalar (or a small nested object/array) inline on one line.
fn inline(v: &Value) -> String {
    match v {
        // One row is one line. A raw fact can carry newlines (a multi-line
        // `use a::{\n b,\n}` import string), so collapse them or the row would
        // silently spill across several lines and break the grep-style contract.
        Value::String(s) if s.contains(['\n', '\r', '\t']) => {
            s.split_whitespace().collect::<Vec<_>>().join(" ")
        }
        Value::String(s) => s.clone(),
        Value::Object(m) => m
            .iter()
            .map(|(k, val)| format!("{k}={}", inline(val)))
            .collect::<Vec<_>>()
            .join(" "),
        Value::Array(a) => a.iter().map(inline).collect::<Vec<_>>().join(","),
        Value::Null => String::new(),
        _ => v.to_string(),
    }
}

/// Retain only `fields` on each result row, wherever the rows live: a top-level
/// array (`files`, `symbols`, …) or an array nested inside the data object
/// (`matches` for `search`, `references`, `chunks` for `context`, the grouped
/// lists in `map`). Scalar/metadata fields on the wrapper object (e.g.
/// `truncated`) are left intact. Non-object rows are untouched.
/// Returns `(unmatched, available)`: the requested fields that appear on no row,
/// and the field names that were actually there. A projection that silently
/// yields a narrower row than asked for reads as "this record has no such value"
/// rather than "you asked for a name that isn't a field", so the caller reports
/// the difference. It is a note and not an error because a field can be
/// *legitimately* absent — `via` only appears under `--resolved`, `col`/`text`
/// only under `--examples` — and rejecting those would break working queries.
fn project(data: &mut Value, fields: &[String]) -> (Vec<String>, Vec<String>) {
    let mut available: std::collections::BTreeSet<String> = Default::default();
    collect_keys(data, &mut available);
    match data {
        Value::Array(rows) => {
            for row in rows {
                project_obj(row, fields);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                if let Value::Array(rows) = value {
                    for row in rows {
                        project_obj(row, fields);
                    }
                }
            }
        }
        _ => {}
    }
    let unmatched: Vec<String> = fields
        .iter()
        .filter(|f| !available.contains(f.as_str()))
        .cloned()
        .collect();
    (unmatched, available.into_iter().collect())
}

/// Every key present on any row, in the same shapes `project` walks.
fn collect_keys(data: &Value, out: &mut std::collections::BTreeSet<String>) {
    let keys_of = |v: &Value, out: &mut std::collections::BTreeSet<String>| {
        if let Value::Object(m) = v {
            out.extend(m.keys().cloned());
        }
    };
    match data {
        Value::Array(rows) => {
            for row in rows {
                keys_of(row, out);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                if let Value::Array(rows) = value {
                    for row in rows {
                        keys_of(row, out);
                    }
                }
            }
        }
        _ => {}
    }
}

fn project_obj(v: &mut Value, fields: &[String]) {
    if let Value::Object(map) = v {
        map.retain(|k, _| fields.iter().any(|f| f == k));
    }
}

#[derive(Serialize)]
pub struct RepoMeta {
    pub root: String,
    pub vcs: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
}

#[derive(Serialize)]
pub struct IndexMeta {
    pub schema: i64,
    pub fresh: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub reindexed_files: u64,
    /// Files that failed to parse/read during this refresh; 0 in the happy path.
    #[serde(skip_serializing_if = "is_zero")]
    pub partial_failures: u64,
    pub elapsed_ms: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}
