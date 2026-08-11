//! The content-search engine shared by `search`, `references`, `unused`, and
//! `astgrep`: pull candidate file bodies from the trigram FTS index, run a regex
//! over them, classify matches by code/comment/string span, and shape the output
//! as matches / files / counts. No filesystem reads — everything comes from the
//! index. Commands in [`crate::commands`] drive it; it depends only on a
//! `rusqlite::Connection`, not the whole session.
//!
//! [`scan_stdin`] is the one exception: it scans a buffer piped in on stdin
//! rather than the index, so codeq can also stand in for grep in a pipeline.
//! It shares [`scan_doc`] with the indexed path, so both answer identically.

use std::io::{IsTerminal, Read};

use anyhow::{anyhow, Result};
use regex_syntax::hir::literal::{ExtractKind, Extractor as LiteralExtractor};
use serde::Serialize;

use crate::callsite::{calls_in, CallMap};
use crate::facts::{has_spans, Extractor, Span};

const MAX_LINE: usize = 240;

/// One matched line, shared by `search` and `references`.
#[derive(Serialize, Clone)]
pub(crate) struct Hit {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) col: usize,
    /// The innermost definition enclosing this match, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) symbol: Option<String>,
    /// For a call site, the receiver it is called on, as written: `db` in
    /// `db.query()`, `helpers` in `helpers::stem()`. Only populated when a
    /// position filter had the file parsed anyway.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) receiver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    /// The matched line, trailing whitespace trimmed and capped.
    pub(crate) text: String,
    /// Context lines before the match (with -B/-C), nearest last.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) before: Vec<String>,
    /// Context lines after the match (with -A/-C).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) after: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct FileCount {
    pub(crate) path: String,
    pub(crate) count: usize,
}

/// Output shape depends on the mode: full matches, matching file paths, or
/// per-file counts. `truncated` is true when `--limit` cut it off.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum SearchData {
    Matches { matches: Vec<Hit>, truncated: bool },
    Files { files: Vec<String>, truncated: bool },
    Counts { counts: Vec<FileCount>, truncated: bool },
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum OutMode {
    Matches,
    Files,
    Count,
}

/// Restrict matches by the syntactic class of their position.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum InFilter {
    Any,
    Code,
    Comments,
    Strings,
    /// Everything except a comment. Not exposed as a `--in` value: it exists for
    /// `unused`, where a mention in a comment must not count as a use, but a
    /// mention in a *string* still can — reflection, `getattr`, serde renames,
    /// DI registration, SQL identifiers and config keys are all real uses that
    /// live in string literals. Excluding comments removes the clearly-wrong
    /// masking without making the command guess about the ambiguous case.
    NotComment,
}

impl InFilter {
    pub(crate) fn parse(s: &str) -> Result<InFilter> {
        Ok(match s {
            "any" => InFilter::Any,
            "code" => InFilter::Code,
            "comment" | "comments" => InFilter::Comments,
            "string" | "strings" => InFilter::Strings,
            other => return Err(anyhow!("--in must be code|comments|strings, got '{other}'")),
        })
    }

    fn accepts(self, cls: Option<Span>) -> bool {
        match self {
            InFilter::Any => true,
            InFilter::Code => cls.is_none(),
            InFilter::Comments => cls == Some(Span::Comment),
            InFilter::Strings => cls == Some(Span::Str),
            InFilter::NotComment => cls != Some(Span::Comment),
        }
    }

    /// Whether a file codeq cannot classify must be skipped entirely.
    ///
    /// True for the `--in` filters, which are a positive claim ("this match is
    /// in a comment") that cannot be made without spans. False for
    /// [`InFilter::NotComment`], which is an exclusion: with no spans nothing is
    /// known to be a comment, so every match stands. That keeps `unused` reading
    /// occurrences in JSON, TOML and other unparsed files as uses, rather than
    /// silently dropping them and reporting live code as dead.
    fn requires_spans(self) -> bool {
        matches!(self, InFilter::Code | InFilter::Comments | InFilter::Strings)
    }
}

/// SQL selecting a file's cached comment/string spans, sorted. Statement text
/// rather than a helper that runs it: both callers prepare once and step it per
/// file, so sharing the query keeps one notion of a span without moving the
/// `prepare` into a hot loop.
pub(crate) const SPANS_SQL: &str =
    "SELECT start_byte, end_byte, kind FROM spans WHERE path = ?1 ORDER BY start_byte";

/// Map a [`SPANS_SQL`] row to `(start, end, class)`.
pub(crate) fn span_row(r: &rusqlite::Row) -> rusqlite::Result<(usize, usize, Span)> {
    Ok((
        r.get::<_, i64>(0)? as usize,
        r.get::<_, i64>(1)? as usize,
        Span::from_i64(r.get::<_, i64>(2)?).unwrap_or(Span::Str),
    ))
}

/// The class of byte offset `off` given sorted, non-overlapping `spans`.
pub(crate) fn class_at(spans: &[(usize, usize, Span)], off: usize) -> Option<Span> {
    let idx = spans.partition_point(|(s, _, _)| *s <= off);
    if idx == 0 {
        return None;
    }
    let (s, e, c) = spans[idx - 1];
    if s <= off && off < e {
        Some(c)
    } else {
        None
    }
}

pub(crate) struct ScanOpts {
    pub(crate) before: usize,
    pub(crate) after: usize,
    pub(crate) out: OutMode,
    pub(crate) in_filter: InFilter,
    pub(crate) limit: usize,
    /// Keep only matches in call position — immediately followed by `(` — so a
    /// bare identifier / field / variable of the same name is excluded.
    pub(crate) call_only: bool,
    /// Keep only matches qualified by this receiver: `RECV.name`, `RECV::name`,
    /// or `RECV->name`. For disambiguating a method/associated name by its type.
    pub(crate) qualifier: Option<String>,
    /// Mirror of `--max-per-file`: matches past the Nth in a file still get
    /// collected (the envelope needs them to report each file's true count) but
    /// they do not spend `limit`. Without this the two budgets fight — the scan
    /// stops after `limit` *raw* matches, so a cap of 3 that should have surveyed
    /// the whole repo instead surveys the first `limit/3` files and reports their
    /// partial counts as totals.
    pub(crate) per_file: Option<usize>,
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Whether the match is a markup instantiation — `<Name …/>` or `</Name>`.
///
/// A Svelte or Vue component is *used* by writing it as an element, which no
/// grammar reports as a call expression: tree-sitter is right that this is not a
/// call node, and codeq is right that it is a usage. So this is checked
/// separately from, and in addition to, the parse tree — otherwise `--calls` on a
/// component returns nothing for a component that is plainly used, which is how
/// this regressed when call detection moved onto the parse tree.
pub(crate) fn is_markup_instantiation(bytes: &[u8], start: usize) -> bool {
    let mut i = start;
    while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
        i -= 1;
    }
    if i > 0 && bytes[i - 1] == b'/' {
        i -= 1; // a closing tag
    }
    i > 0 && bytes[i - 1] == b'<'
}

/// Whether the match is an *invocation* rather than a bare mention.
///
/// `Name(` is the call-syntax languages. But codeq now indexes formats where
/// invocation is spelled differently and `Name(` never appears: a Svelte or Vue
/// component is invoked as `<Name …/>`, so `--calls` on one used to return
/// nothing at all for a component that is plainly used. Both forms count, since
/// the question `--calls` answers — "where is this actually used, not merely
/// named?" — is the same one.
pub(crate) fn is_call_position(bytes: &[u8], start: usize, end: usize) -> bool {
    if is_markup_instantiation(bytes, start) {
        return true;
    }
    let mut j = end;
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    j < bytes.len() && bytes[j] == b'('
}

/// Whether the match starting at byte `off` is qualified by `recv` — preceded by
/// `recv.` / `recv::` / `recv->` at a word boundary.
///
/// Whitespace, including newlines, is skipped on both sides of the separator, so
/// a wrapped method chain reads the same as a single-line call:
///
/// ```text
/// db.query()          db . query()          db
///                                               .query()
/// ```
///
/// Still textual rather than a parsed receiver expression: `a.b.query()` counts
/// for `--on b` (the nearest qualifier is what is tested) and a call-valued
/// receiver like `conn().query()` is not matched at all.
fn has_qualifier(bytes: &[u8], off: usize, recv: &str) -> bool {
    let skip_ws_back = |mut i: usize| {
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        i
    };

    let q = recv.as_bytes();
    let sep_end = skip_ws_back(off);
    for sep in [b"::".as_slice(), b"->".as_slice(), b".".as_slice()] {
        if sep_end < sep.len() || &bytes[sep_end - sep.len()..sep_end] != sep {
            continue;
        }
        let qend = skip_ws_back(sep_end - sep.len());
        if qend >= q.len()
            && &bytes[qend - q.len()..qend] == q
            && (qend == q.len() || !is_ident_byte(bytes[qend - q.len() - 1]))
        {
            return true;
        }
    }
    false
}

pub(crate) struct FileDef {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
}

/// Byte offset of the start of each line.
fn line_starts_of(text: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, b) in text.as_bytes().iter().enumerate() {
        if *b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// The text of line `idx` (0-based), trailing whitespace trimmed and capped.
fn line_text(text: &str, line_starts: &[usize], idx: usize) -> String {
    let start = line_starts[idx];
    let end = line_starts.get(idx + 1).map(|&s| s - 1).unwrap_or(text.len());
    cap_str(text[start..end.min(text.len())].trim_end(), MAX_LINE)
}

/// Context lines for the indices in `range` that exist.
fn context_lines(
    text: &str,
    line_starts: &[usize],
    range: std::ops::Range<usize>,
) -> Vec<String> {
    range
        .filter(|&i| i < line_starts.len())
        .map(|i| line_text(text, line_starts, i))
        .collect()
}

pub(crate) fn load_file_defs(
    stmt: &mut rusqlite::Statement,
    path: &str,
) -> Result<Vec<FileDef>> {
    let defs = stmt
        .query_map([path], |r| {
            Ok(FileDef {
                name: r.get(0)?,
                kind: r.get(1)?,
                start_byte: r.get::<_, i64>(2)? as usize,
                end_byte: r.get::<_, i64>(3)? as usize,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(defs)
}

/// The innermost definition whose byte range contains `off`.
pub(crate) fn innermost(defs: &[FileDef], off: usize) -> Option<&FileDef> {
    defs.iter()
        .filter(|d| d.start_byte <= off && off < d.end_byte)
        .max_by_key(|d| d.start_byte)
}

/// Run `re` over candidate bodies and shape the output per `opts.out`.
/// Deterministic order (path, then line). When `opts.in_filter` is not `Any`,
/// matches are classified via cached spans; files in a language we cannot parse
/// are excluded (never guessed).
/// Stream candidate file bodies from the trigram FTS index and run `re` over each,
/// shaping the output per `opts.out`. Deterministic order (path, then line).
///
/// Bodies are *borrowed* from each SQLite row and scanned in place — never
/// collected — so a query over a common substring (which can match a large
/// fraction of a big repo) holds one body at a time instead of materializing all
/// of them (which spiked memory into the hundreds of MB and copied every byte).
/// The trigram `LIKE` prefilter is a safe superset (`_`/`%` only broaden); the
/// precise regex confirms. With no literal, every text file is a candidate.
/// `path`/`language` filters apply per row; when `opts.in_filter` isn't `Any`,
/// matches are classified via cached spans (files in an unparseable language are
/// skipped, never guessed).
pub(crate) fn run_search(
    conn: &rusqlite::Connection,
    re: &regex::Regex,
    like: Option<&[String]>,
    path_filter: Option<&str>,
    language_filter: Option<&str>,
    path_accept: Option<&dyn Fn(&str) -> bool>,
    opts: &ScanOpts,
) -> Result<SearchData> {
    let cap = if opts.limit == 0 { usize::MAX } else { opts.limit };
    let filtering = opts.in_filter != InFilter::Any;

    let patterns = like.filter(|l| !l.is_empty());
    let mut sql = String::from("SELECT path, language, body FROM content_fts");
    if let Some(pats) = patterns {
        let clause = (1..=pats.len())
            .map(|i| format!("body LIKE ?{i}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        sql.push_str(&format!(" WHERE {clause}"));
    }
    sql.push_str(" ORDER BY path");
    let params: Vec<&dyn rusqlite::ToSql> = patterns
        .map(|pats| pats.iter().map(|p| p as &dyn rusqlite::ToSql).collect())
        .unwrap_or_default();

    let mut def_stmt = conn.prepare(
        "SELECT name, kind, start_byte, end_byte FROM facts
         WHERE path = ?1 AND kind <> 'import'",
    )?;
    // Comment/string spans are cached in the index (extracted once), so --in is
    // a lookup, not a per-query re-parse.
    let mut span_stmt = conn.prepare(SPANS_SQL)?;

    let mut acc = ScanAcc::default();

    let mut fts_stmt = conn.prepare(&sql)?;
    let mut rows = fts_stmt.query(params.as_slice())?;
    'outer: while let Some(row) = rows.next()? {
        let rel = row.get_ref(0)?.as_str()?;
        if let Some(pf) = path_filter {
            if !rel.contains(pf) {
                continue;
            }
        }
        if let Some(accept) = path_accept {
            if !accept(rel) {
                continue;
            }
        }
        let lang: Option<String> = row.get(1)?;
        if let Some(lf) = language_filter {
            if lang.as_deref() != Some(lf) {
                continue;
            }
        }
        // Borrow the body from the row — never copied into an owned String.
        let body = row.get_ref(2)?.as_str()?;

        // Load cached spans if filtering; skip files in a language we can't parse.
        let spans = if filtering {
            if !has_spans(lang.as_deref().unwrap_or("")) {
                if opts.in_filter.requires_spans() {
                    continue;
                }
                // Unclassifiable: treat every position as code rather than drop
                // the file, so an exclusion filter cannot lose real matches.
                Vec::new()
            } else {
                let srows = span_stmt.query_map([rel], span_row)?;
                srows.collect::<std::result::Result<Vec<_>, _>>()?
            }
        } else {
            Vec::new()
        };
        // Defs are only loaded once a match survives the filters, so a candidate
        // that matches the substring but not `\bword\b` (or is filtered by
        // --calls/--on) costs neither a body scan nor a defs query.
        // One parse per candidate file, and only when `--calls`/`--on` is in
        // play: ordinary searches never reach this.
        let calls = if opts.call_only || opts.qualifier.is_some() {
            calls_in(lang.as_deref().unwrap_or(""), body)
        } else {
            None
        };
        let mut load_defs =
            |path: &str| load_file_defs(&mut def_stmt, path).unwrap_or_default();
        if scan_doc(
            rel,
            body,
            &spans,
            calls.as_ref(),
            &mut load_defs,
            re,
            opts,
            cap,
            &mut acc,
        ) {
            break 'outer;
        }
    }

    Ok(acc.finish(opts.out))
}

/// Results accumulated across the documents a scan visits.
#[derive(Default)]
pub(crate) struct ScanAcc {
    files: Vec<String>,
    counts: Vec<FileCount>,
    hits: Vec<Hit>,
    truncated: bool,
    /// Hits that count against `limit`. Equal to `hits.len()` unless
    /// `ScanOpts::per_file` is set, when the overflow a file keeps only so its
    /// true count can be reported is excluded.
    kept: usize,
}

impl ScanAcc {
    pub(crate) fn finish(self, out: OutMode) -> SearchData {
        let ScanAcc {
            files,
            counts,
            hits,
            truncated,
            ..
        } = self;
        match out {
            OutMode::Files => SearchData::Files { files, truncated },
            OutMode::Count => SearchData::Counts { counts, truncated },
            OutMode::Matches => SearchData::Matches {
                matches: hits,
                truncated,
            },
        }
    }
}

/// Scan one document — an indexed file, or a buffer piped in on stdin — for
/// `re`, applying every [`ScanOpts`] filter and appending to `acc`. Returns true
/// when the limit is reached and the caller should stop.
///
/// This is the single place matching happens, so a piped buffer and an indexed
/// file are guaranteed to answer identically: same regex semantics, same
/// `--in`/`--calls`/`--on` filters, same one-row-per-line rule, same context
/// lines. The only difference is where `spans` and `defs` come from.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_doc(
    path: &str,
    body: &str,
    spans: &[(usize, usize, Span)],
    calls: Option<&CallMap>,
    load_defs: &mut dyn FnMut(&str) -> Vec<FileDef>,
    re: &regex::Regex,
    opts: &ScanOpts,
    cap: usize,
    acc: &mut ScanAcc,
) -> bool {
    let filtering = opts.in_filter != InFilter::Any;
    let accept = |off: usize| !filtering || opts.in_filter.accepts(class_at(spans, off));
    // Line starts are only needed once there's a match (Files mode never needs
    // them). Compute lazily, so a non-matching candidate — and every candidate
    // in Files mode — skips the full-body newline scan.
    let mut starts: Option<Vec<usize>> = None;

    match opts.out {
        OutMode::Files => {
            if re.find_iter(body).any(|m| accept(m.start())) {
                acc.files.push(path.to_string());
                if acc.files.len() >= cap {
                    acc.truncated = true;
                    return true;
                }
            }
        }
        OutMode::Count => {
            let mut last = usize::MAX;
            let mut n = 0usize;
            for m in re.find_iter(body) {
                if !accept(m.start()) {
                    continue;
                }
                let s = starts.get_or_insert_with(|| line_starts_of(body));
                let li = s.partition_point(|&x| x <= m.start()) - 1;
                if li != last {
                    last = li;
                    n += 1;
                }
            }
            if n > 0 {
                acc.counts.push(FileCount {
                    path: path.to_string(),
                    count: n,
                });
                if acc.counts.len() >= cap {
                    acc.truncated = true;
                    return true;
                }
            }
        }
        OutMode::Matches => {
            let bytes = body.as_bytes();
            let mut defs: Option<Vec<FileDef>> = None;
            let mut last_line = usize::MAX;
            let mut kept_here = 0usize;
            let mut stop = false;
            for m in re.find_iter(body) {
                let off = m.start();
                if !accept(off) {
                    continue;
                }
                // With a parse of this file, "is this a call" and "what is it
                // called on" are answered by the tree. Without one (no grammar
                // for the language), fall back to the byte-level tests rather
                // than drop every hit.
                match calls {
                    Some(map) => {
                        if opts.call_only
                            && !map.contains_key(&off)
                            && !is_markup_instantiation(bytes, off)
                        {
                            continue;
                        }
                        if let Some(recv) = &opts.qualifier {
                            if map.get(&off).and_then(|r| r.as_deref()) != Some(recv.as_str()) {
                                continue;
                            }
                        }
                    }
                    None => {
                        // The byte-level fallback takes the match *start* as well
                        // as its end: a Svelte/Vue component is invoked as
                        // `<Name …/>`, which is only detectable by looking left of
                        // the name. Those languages have no grammar for `calls_in`
                        // to parse, so this fallback is the only path they take.
                        if opts.call_only && !is_call_position(bytes, m.start(), m.end()) {
                            continue;
                        }
                        if let Some(recv) = &opts.qualifier {
                            if !has_qualifier(bytes, off, recv) {
                                continue;
                            }
                        }
                    }
                }
                let s = starts.get_or_insert_with(|| line_starts_of(body));
                let line_idx = s.partition_point(|&x| x <= off) - 1;
                if line_idx == last_line {
                    continue; // one row per line
                }
                last_line = line_idx;

                let d = defs.get_or_insert_with(|| load_defs(path));
                let enclosing = innermost(d, off);
                acc.hits.push(Hit {
                    path: path.to_string(),
                    line: line_idx + 1,
                    col: off - s[line_idx],
                    symbol: enclosing.map(|d| d.name.clone()),
                    receiver: calls.and_then(|m| m.get(&off).cloned()).flatten(),
                    kind: enclosing.map(|d| d.kind.clone()),
                    text: line_text(body, s, line_idx),
                    before: context_lines(body, s, line_idx.saturating_sub(opts.before)..line_idx),
                    after: context_lines(body, s, (line_idx + 1)..(line_idx + 1 + opts.after)),
                });
                // Past this file's cap the hit is carried for its count only, so
                // it must not consume `limit` — otherwise the survey stops early
                // on rows nobody will ever see.
                if opts.per_file.is_none_or(|n| kept_here < n) {
                    kept_here += 1;
                    acc.kept += 1;
                    if acc.kept >= cap {
                        acc.truncated = true;
                        // Under a per-file cap, stopping mid-file would report
                        // this file's partial count as its total — the one file
                        // in the survey that lies. Finish counting it, then stop.
                        if opts.per_file.is_none() {
                            return true;
                        }
                        stop = true;
                    }
                }
            }
            if stop {
                return true;
            }
        }
    }
    false
}

/// The label used for the piped buffer where a result would otherwise carry a
/// path. Parenthesized like `map`'s `(root)`, so it can't be mistaken for a
/// file named `stdin` and stays greppable.
pub(crate) const STDIN_LABEL: &str = "(stdin)";

/// Scan a buffer piped in on stdin instead of the index.
///
/// Never inferred from whether stdin is a TTY: an agent's shell usually has
/// stdin closed or on /dev/null, so auto-detection would silently turn every
/// ordinary `codeq search` into a search of an empty buffer. The caller must
/// opt in explicitly, and this is the only way to reach this function.
///
/// `language` is what buys anything over grep: with it, the buffer is parsed
/// once with Tree-sitter, which gives `--in code|comments|strings` and the
/// enclosing `[symbol]` on each match. Without it the scan is still exact, but
/// those two are unavailable, and `--in` is rejected rather than silently
/// ignored.
pub(crate) fn scan_stdin(
    re: &regex::Regex,
    opts: &ScanOpts,
    language: Option<&str>,
    label: &str,
) -> Result<SearchData> {
    // Nothing is piped in, so reading would block forever. grep would hang here;
    // for an agent a hang is worse than an error, so say what went wrong.
    if std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "--stdin given but nothing is piped in (stdin is a terminal)"
        ));
    }
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf)?;
    // Lossy rather than an error: piped input is often a log or a mixed-encoding
    // file, and a stray byte shouldn't lose the whole search.
    let body = String::from_utf8_lossy(&buf).into_owned();

    let filtering = opts.in_filter != InFilter::Any;
    let (mut defs, spans) = match language {
        Some(lang) => {
            if filtering && !has_spans(lang) {
                return Err(anyhow!(
                    "--in needs comment/string spans, which codeq does not extract for '{lang}'"
                ));
            }
            let mut ex = Extractor::new()?;
            // No path: stdin has no filename. Only single-file component
            // formats use it (to name the component after its file), and a
            // piped Svelte buffer has no file to be named after.
            let (facts, spans, _) = ex.extract(lang, "", body.as_bytes());
            let defs: Vec<FileDef> = facts
                .into_iter()
                .filter(|f| f.kind != "import")
                .map(|f| FileDef {
                    name: f.name,
                    kind: f.kind,
                    start_byte: f.start_byte,
                    end_byte: f.end_byte,
                })
                .collect();
            (defs, spans)
        }
        None => {
            if filtering {
                return Err(anyhow!(
                    "--in needs to know the language of piped input: add --language <lang>"
                ));
            }
            (Vec::new(), Vec::new())
        }
    };

    let cap = if opts.limit == 0 { usize::MAX } else { opts.limit };
    let calls = if opts.call_only || opts.qualifier.is_some() {
        language.and_then(|l| calls_in(l, &body))
    } else {
        None
    };
    let mut acc = ScanAcc::default();
    let mut load_defs = |_: &str| std::mem::take(&mut defs);
    scan_doc(
        label,
        &body,
        &spans,
        calls.as_ref(),
        &mut load_defs,
        re,
        opts,
        cap,
        &mut acc,
    );
    Ok(acc.finish(opts.out))
}

/// A `%literal%` LIKE pattern for the content index — trigram needs >= 3 chars,
/// so shorter literals can't be prefiltered (caller full-scans). `_`/`%` in the
/// literal are left as LIKE wildcards: only broadens the candidate set (a safe
/// superset), and the precise regex confirms.
pub(crate) fn like_pattern(lit: &str) -> Option<String> {
    if lit.chars().count() < 3 {
        return None;
    }
    Some(format!("%{lit}%"))
}

/// A single-literal prefilter (for `--fixed` search and `references`).
pub(crate) fn like_of(lit: &str) -> Option<Vec<String>> {
    like_pattern(lit).map(|p| vec![p])
}

/// Build a prefilter for a regex: a set of literals such that *every* match must
/// contain at least one of them (as a substring), OR-ed as LIKE patterns. Uses
/// regex-syntax prefix/suffix literal extraction, then an inner-literal fallback.
/// Returns `None` when no safe prefilter exists — the caller then full-scans,
/// which is always correct.
///
/// Correctness: a finite prefix (or suffix) literal set means every match starts
/// (or ends) with one of the literals, so it contains one as a substring; the
/// LIKE OR is a true superset and never drops a match.
pub(crate) fn regex_prefilter(pattern: &str) -> Option<Vec<String>> {
    let hir = regex_syntax::parse(pattern).ok()?;
    usable_literals(ExtractKind::Prefix, &hir)
        .or_else(|| usable_literals(ExtractKind::Suffix, &hir))
        .or_else(|| inner_required_literal(&hir))
}

/// The longest literal that must appear in *every* match, found by walking the
/// HIR. This catches a literal buried in the middle (e.g. `.*needle.*`) that
/// prefix/suffix extraction misses. A required literal is a safe single-term
/// prefilter; when there isn't one >= 3 chars we return None and full-scan.
fn inner_required_literal(hir: &regex_syntax::hir::Hir) -> Option<Vec<String>> {
    fn flush(cur: &mut Vec<u8>, best: &mut Vec<u8>) {
        if cur.len() > best.len() {
            *best = cur.clone();
        }
        cur.clear();
    }
    // Accumulate contiguous literals; anything that isn't an unconditional
    // literal (a class, an optional/zero repetition, an alternation, a look)
    // breaks the run. Required containers (concat, capture, min>=1 repetition)
    // are walked so their literals count too.
    fn walk(hir: &regex_syntax::hir::Hir, cur: &mut Vec<u8>, best: &mut Vec<u8>) {
        use regex_syntax::hir::HirKind;
        match hir.kind() {
            HirKind::Literal(lit) => cur.extend_from_slice(&lit.0[..]),
            HirKind::Concat(subs) => {
                for s in subs {
                    walk(s, cur, best);
                }
            }
            HirKind::Capture(c) => walk(&c.sub, cur, best),
            HirKind::Repetition(r) if r.min >= 1 => {
                flush(cur, best);
                walk(&r.sub, cur, best);
                flush(cur, best);
            }
            _ => flush(cur, best),
        }
    }
    let mut best = Vec::new();
    let mut cur = Vec::new();
    walk(hir, &mut cur, &mut best);
    flush(&mut cur, &mut best);
    let s = std::str::from_utf8(&best).ok()?;
    like_pattern(s).map(|p| vec![p])
}

fn usable_literals(kind: ExtractKind, hir: &regex_syntax::hir::Hir) -> Option<Vec<String>> {
    let mut ex = LiteralExtractor::new();
    ex.kind(kind);
    let seq = ex.extract(hir);
    let lits = seq.literals()?;
    if lits.is_empty() || lits.len() > 64 {
        return None;
    }
    let mut terms = Vec::with_capacity(lits.len());
    for lit in lits {
        let s = std::str::from_utf8(lit.as_bytes()).ok()?;
        // Any alternative shorter than a trigram is unfilterable, which would
        // make the whole OR unsafe — bail to a full scan instead.
        terms.push(like_pattern(s)?);
    }
    Some(terms)
}

/// Truncate `s` to `max` chars, appending an ellipsis when cut.
fn cap_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}
