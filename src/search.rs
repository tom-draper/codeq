//! The content-search engine shared by `search`, `references`, `unused`, and
//! `astgrep`: pull candidate file bodies from the trigram FTS index, run a regex
//! over them, classify matches by code/comment/string span, and shape the output
//! as matches / files / counts. No filesystem reads — everything comes from the
//! index. Commands in [`crate::commands`] drive it; it depends only on a
//! `rusqlite::Connection`, not the whole session.

use anyhow::{anyhow, Result};
use regex_syntax::hir::literal::{ExtractKind, Extractor as LiteralExtractor};
use serde::Serialize;

use crate::facts::{has_spans, Span};

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
        }
    }
}

/// The class of byte offset `off` given sorted, non-overlapping `spans`.
fn class_at(spans: &[(usize, usize, Span)], off: usize) -> Option<Span> {
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
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Whether the match ending at byte `end` is immediately followed by `(` (after
/// optional spaces/tabs) — i.e. it's a call, not a bare mention.
fn is_call_position(bytes: &[u8], end: usize) -> bool {
    let mut j = end;
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    j < bytes.len() && bytes[j] == b'('
}

/// Whether the match starting at byte `off` is qualified by `recv` — preceded by
/// `recv.` / `recv::` / `recv->` at a word boundary.
fn has_qualifier(bytes: &[u8], off: usize, recv: &str) -> bool {
    let q = recv.as_bytes();
    for sep in [b"::".as_slice(), b"->".as_slice(), b".".as_slice()] {
        if off < sep.len() || &bytes[off - sep.len()..off] != sep {
            continue;
        }
        let qend = off - sep.len();
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
    let mut span_stmt = conn.prepare(
        "SELECT start_byte, end_byte, kind FROM spans WHERE path = ?1 ORDER BY start_byte",
    )?;

    let mut files: Vec<String> = Vec::new();
    let mut counts: Vec<FileCount> = Vec::new();
    let mut hits: Vec<Hit> = Vec::new();
    let mut truncated = false;

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
                continue;
            }
            let srows = span_stmt.query_map([rel], |r| {
                Ok((
                    r.get::<_, i64>(0)? as usize,
                    r.get::<_, i64>(1)? as usize,
                    Span::from_i64(r.get::<_, i64>(2)?).unwrap_or(Span::Str),
                ))
            })?;
            srows.collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let accept = |off: usize| !filtering || opts.in_filter.accepts(class_at(&spans, off));
        let starts = line_starts_of(body);

        match opts.out {
            OutMode::Files => {
                if re.find_iter(body).any(|m| accept(m.start())) {
                    files.push(rel.to_string());
                    if files.len() >= cap {
                        truncated = true;
                        break 'outer;
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
                    let li = starts.partition_point(|&s| s <= m.start()) - 1;
                    if li != last {
                        last = li;
                        n += 1;
                    }
                }
                if n > 0 {
                    counts.push(FileCount {
                        path: rel.to_string(),
                        count: n,
                    });
                    if counts.len() >= cap {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
            OutMode::Matches => {
                let defs = load_file_defs(&mut def_stmt, rel)?;
                let bytes = body.as_bytes();
                let mut last_line = usize::MAX;
                for m in re.find_iter(body) {
                    let off = m.start();
                    if !accept(off) {
                        continue;
                    }
                    if opts.call_only && !is_call_position(bytes, m.end()) {
                        continue;
                    }
                    if let Some(recv) = &opts.qualifier {
                        if !has_qualifier(bytes, off, recv) {
                            continue;
                        }
                    }
                    let line_idx = starts.partition_point(|&s| s <= off) - 1;
                    if line_idx == last_line {
                        continue; // one row per line
                    }
                    last_line = line_idx;

                    let enclosing = innermost(&defs, off);
                    hits.push(Hit {
                        path: rel.to_string(),
                        line: line_idx + 1,
                        col: off - starts[line_idx],
                        symbol: enclosing.map(|d| d.name.clone()),
                        kind: enclosing.map(|d| d.kind.clone()),
                        text: line_text(body, &starts, line_idx),
                        before: context_lines(
                            body,
                            &starts,
                            line_idx.saturating_sub(opts.before)..line_idx,
                        ),
                        after: context_lines(
                            body,
                            &starts,
                            (line_idx + 1)..(line_idx + 1 + opts.after),
                        ),
                    });
                    if hits.len() >= cap {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
        }
    }

    Ok(match opts.out {
        OutMode::Files => SearchData::Files { files, truncated },
        OutMode::Count => SearchData::Counts { counts, truncated },
        OutMode::Matches => SearchData::Matches {
            matches: hits,
            truncated,
        },
    })
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
